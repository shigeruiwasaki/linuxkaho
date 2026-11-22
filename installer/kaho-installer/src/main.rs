use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::Command;
use std::{thread, time::Duration};

slint::include_modules!();

const LOG_PATH: &str = "/tmp/kaho-installer.log";

/// 進行画面に表示する手順(タイトル, ひとこと)
const STEPS: &[(&str, &str)] = &[
    ("ディスクを準備しています", "新しい環境の置き場所を整えています。"),
    ("システムをコピーしています", "少し時間がかかります。のんびりお待ちください。"),
    ("あなたのアカウントを作っています", "お名前とパスワードを設定しています。"),
    ("起動の仕組みを整えています", "次回から迷わず起動できるようにしています。"),
    ("仕上げをしています", "もうすぐ完了です。"),
];

#[allow(dead_code)] // 本番のインストール処理(Stage 2)で使用
struct Config {
    real: bool,
    disk: String,
    full_name: String,
    username: String,
    password: String,
}

fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = writeln!(f, "{msg}");
    }
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1000.0 { format!("{:.1} TB", gb / 1000.0) } else { format!("{:.0} GB", gb) }
}

/// lsblk -P の出力(KEY="value" ...)から値を取り出す
fn field(line: &str, key: &str) -> String {
    let hay = format!(" {line}");
    let pat = format!(" {key}=\"");
    hay.find(&pat)
        .map(|i| hay[i + pat.len()..].split('"').next().unwrap_or("").replace("\\x20", " "))
        .unwrap_or_default()
}

fn list_disks() -> Result<Vec<DiskItem>, String> {
    let out = Command::new("lsblk")
        .args(["-b", "-d", "-n", "-P", "-o", "NAME,SIZE,MODEL,TYPE,RM"])
        .output()
        .map_err(|e| format!("ディスクの一覧を取得できませんでした({e})"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let live = live_disk();
    Ok(text
        .lines()
        .filter(|l| field(l, "TYPE") == "disk" && !field(l, "NAME").starts_with("zram") && Some(field(l, "NAME")) != live)
        .map(|l| {
            let name = field(l, "NAME");
            let model = field(l, "MODEL");
            let model = model.trim();
            let size = human(field(l, "SIZE").parse().unwrap_or(0));
            let removable = if field(l, "RM") == "1" { "・取り外し可能" } else { "" };
            DiskItem {
                device: format!("/dev/{name}").into(),
                title: (if model.is_empty() { name.clone() } else { model.to_string() }).into(),
                detail: format!("{size} ・ /dev/{name}{removable}").into(),
            }
        })
        .collect())
}

fn valid_username(u: &str) -> bool {
    let mut chars = u.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && u.len() <= 32
        && u.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn show_error(ui: &MainWindow, title: &str, text: &str) {
    let s = ui.global::<AppState>();
    s.set_error_title(title.into());
    s.set_error_text(text.into());
    s.set_step(6);
}

use std::path::Path;

const TARGET: &str = "/target";
const SQUASHFS: &str = "/run/live/medium/live/filesystem.squashfs";

/// 実際にディスクへ書き込むのは「ライブ環境 かつ root」のときだけ(開発機の保護)。
/// 仮想マシンで試すときなど強制する場合は KAHO_REAL=1、常に模擬にする場合は KAHO_DRY_RUN=1。
fn is_real_mode() -> bool {
    if std::env::var_os("KAHO_DRY_RUN").is_some() {
        return false;
    }
    if std::env::var_os("KAHO_REAL").is_some() {
        return true;
    }
    let live = std::fs::read_to_string("/proc/cmdline").map(|c| c.contains("boot=live")).unwrap_or(false);
    let root = Command::new("id").arg("-u").output().map(|o| o.stdout == b"0\n").unwrap_or(false);
    live && root
}

/// インストール用USB(ライブ媒体)があるディスク名。誤って消さないよう一覧から除外する。
fn live_disk() -> Option<String> {
    let src = Command::new("findmnt").args(["-n", "-o", "SOURCE", "/run/live/medium"]).output().ok()?;
    let dev = String::from_utf8_lossy(&src.stdout).trim().to_string();
    if dev.is_empty() {
        return None;
    }
    let p = Command::new("lsblk").args(["-n", "-o", "PKNAME", dev.as_str()]).output().ok()?;
    let name = String::from_utf8_lossy(&p.stdout).trim().to_string();
    if name.is_empty() { dev.rsplit('/').next().map(String::from) } else { Some(name) }
}

/// コマンドを実行。標準出力・標準エラーは画面に出さずログへ。
fn sh(prog: &str, args: &[&str]) -> Result<(), String> {
    log(&format!("$ {prog} {}", args.join(" ")));
    let out = Command::new(prog).args(args).output().map_err(|e| format!("{prog} を実行できませんでした: {e}"))?;
    log(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(()) } else { Err(format!("{prog} が失敗しました(終了コード {:?})", out.status.code())) }
}

fn chroot(args: &[&str]) -> Result<(), String> {
    let mut a = vec![TARGET];
    a.extend_from_slice(args);
    sh("chroot", &a)
}

fn part(disk: &str, n: u8) -> String {
    // nvme0n1 / mmcblk0 は p1、sda は 1
    if disk.chars().last().map_or(false, |c| c.is_ascii_digit()) { format!("{disk}p{n}") } else { format!("{disk}{n}") }
}

fn uuid(dev: &str) -> Result<String, String> {
    let o = Command::new("blkid").args(["-s", "UUID", "-o", "value", dev]).output().map_err(|e| e.to_string())?;
    let u = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if u.is_empty() { Err(format!("{dev} のUUIDを取得できませんでした")) } else { Ok(u) }
}

fn set_password(cfg: &Config) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = Command::new("chroot")
        .args([TARGET, "chpasswd"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut si) = child.stdin.take() {
        writeln!(si, "{}:{}", cfg.username, cfg.password).map_err(|e| e.to_string())?;
    }
    if child.wait().map_err(|e| e.to_string())?.success() { Ok(()) } else { Err("パスワードを設定できませんでした".into()) }
}

/// 本番のインストール手順(UEFI / GPT / ディスク全体を使用)。STEPS の番号に対応。
fn real_step(i: usize, cfg: &Config) -> Result<(), String> {
    let d = cfg.disk.as_str();
    let (efi, root) = (part(d, 1), part(d, 2));
    let efi_mnt = format!("{TARGET}/boot/efi");
    let e = |x: std::io::Error| x.to_string();
    match i {
        0 => {
            // 消す前に、実行できる環境かを確認する
            if !Path::new("/sys/firmware/efi").exists() {
                return Err("UEFIで起動していません".into());
            }
            if !Path::new(SQUASHFS).exists() {
                return Err("インストール用のシステムイメージが見つかりません".into());
            }
            let _ = sh("sh", &["-c", format!("umount {d}* 2>/dev/null || true").as_str()]);
            sh("wipefs", &["-a", d])?;
            sh("parted", &["-s", d, "mklabel", "gpt", "mkpart", "ESP", "fat32", "1MiB", "513MiB", "set", "1", "esp", "on", "mkpart", "kaho", "ext4", "513MiB", "100%"])?;
            let _ = sh("partprobe", &[d]);
            sh("udevadm", &["settle"])?;
            sh("mkfs.fat", &["-F", "32", "-n", "EFI", efi.as_str()])?;
            sh("mkfs.ext4", &["-F", "-L", "kaho", root.as_str()])?;
            std::fs::create_dir_all(TARGET).map_err(e)?;
            sh("mount", &[root.as_str(), TARGET])?;
            std::fs::create_dir_all(&efi_mnt).map_err(e)?;
            sh("mount", &[efi.as_str(), efi_mnt.as_str()])
        }
        1 => sh("unsquashfs", &["-f", "-n", "-d", TARGET, SQUASHFS]),
        2 => {
            let fstab = format!(
                "UUID={} /boot/efi vfat umask=0077 0 2\nUUID={} / ext4 errors=remount-ro 0 1\n",
                uuid(&efi)?,
                uuid(&root)?
            );
            std::fs::write(format!("{TARGET}/etc/fstab"), fstab).map_err(e)?;
            std::fs::write(format!("{TARGET}/etc/hostname"), "kaho\n").map_err(e)?;
            let gecos = cfg.full_name.replace(|c: char| matches!(c, ',' | ':' | '\n'), "");
            chroot(&["useradd", "-m", "-s", "/bin/bash", "-c", gecos.as_str(), cfg.username.as_str()])?;
            set_password(cfg)?;
            let _ = chroot(&["usermod", "-aG", "sudo", cfg.username.as_str()]);
            Ok(())
        }
        3 => {
            for m in ["dev", "proc", "sys", "run"] {
                let to = format!("{TARGET}/{m}");
                sh("mount", &["--rbind", format!("/{m}").as_str(), to.as_str()])?;
                sh("mount", &["--make-rslave", to.as_str()])?; // 解除がホスト側に影響しないように
            }
            chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--bootloader-id=kaho", "--recheck"])?;
            let _ = chroot(&["grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--removable"]);
            chroot(&["update-initramfs", "-u", "-k", "all"])?;
            chroot(&["update-grub"])
        }
        _ => sh("sync", &[]),
    }
}

fn run_step(index: usize, cfg: &Config) -> Result<(), String> {
    if cfg.real {
        return real_step(index, cfg);
    }
    // 模擬モード(ディスクには書き込まない)
    thread::sleep(Duration::from_millis(1500));
    if index == 2 && std::env::var_os("KAHO_SIMULATE_ERROR").is_some() {
        return Err("テスト用のエラーです".to_string());
    }
    Ok(())
}

fn install(weak: &slint::Weak<MainWindow>, cfg: &Config) -> Result<(), String> {
    let total = STEPS.len() as f32;
    let result = (|| -> Result<(), String> {
        for (i, (title, note)) in STEPS.iter().enumerate() {
            let (title, note, p) = (*title, *note, i as f32 / total);
            weak.upgrade_in_event_loop(move |ui| {
                let s = ui.global::<AppState>();
                s.set_status_title(title.into());
                s.set_status_text(note.into());
                s.set_progress(p);
            })
            .ok();
            run_step(i, cfg)?;
        }
        Ok(())
    })();
    if cfg.real {
        // 成功・失敗にかかわらず後片付け
        let _ = sh("sync", &[]);
        let _ = sh("umount", &["-R", TARGET]);
    }
    result
}

fn run() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let state = ui.global::<AppState>();

    let w = ui.as_weak();
    state.on_validate_account(move || {
        let ui = w.unwrap();
        let s = ui.global::<AppState>();
        let (name, user, p1, p2) = (s.get_full_name(), s.get_username(), s.get_password(), s.get_password2());
        let msg = if name.trim().is_empty() {
            "お名前を入力してください。"
        } else if !valid_username(&user) {
            "ユーザー名は、半角の小文字・数字・「-」「_」で、小文字か「_」から始めてください。"
        } else if p1.chars().count() < 8 {
            "パスワードは8文字以上にしてください。"
        } else if p1 != p2 {
            "確認用のパスワードが一致しません。もう一度入力してください。"
        } else {
            ""
        };
        msg.into()
    });

    let w = ui.as_weak();
    state.on_refresh_disks(move || {
        let ui = w.unwrap();
        match list_disks() {
            Ok(v) => {
                let s = ui.global::<AppState>();
                s.set_disks(ModelRc::new(VecModel::from(v)));
                s.set_selected_disk(-1);
            }
            Err(e) => {
                log(&e);
                show_error(&ui, "ディスクを確認できませんでした", &format!("もう一度お試しください。詳しい情報は {LOG_PATH} に保存しました。"));
            }
        }
    });

    let w = ui.as_weak();
    state.on_start_install(move || {
        let ui = w.unwrap();
        let s = ui.global::<AppState>();
        let idx = s.get_selected_disk();
        let disk = if idx >= 0 { s.get_disks().row_data(idx as usize) } else { None };
        let Some(disk) = disk else {
            show_error(&ui, "インストール先を確認できませんでした", "インストール先をもう一度選び直してください。");
            return;
        };
        let cfg = Config {
            real: is_real_mode(),
            disk: disk.device.to_string(),
            full_name: s.get_full_name().to_string(),
            username: s.get_username().to_string(),
            password: s.get_password().to_string(),
        };
        s.set_progress(0.0);
        s.set_step(4);

        let weak = ui.as_weak();
        thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| install(&weak, &cfg)))
                .unwrap_or_else(|_| Err("予期しない問題が発生しました".to_string()));
            weak.upgrade_in_event_loop(move |ui| match outcome {
                Ok(()) => ui.global::<AppState>().set_step(5),
                Err(e) => {
                    log(&e);
                    show_error(
                        &ui,
                        "うまくいきませんでした",
                        &format!("インストールを完了できませんでした。お手数ですが、もう一度お試しください。詳しい情報は {LOG_PATH} に保存しました。"),
                    );
                }
            })
            .ok();
        });
    });

    // 開発中は再起動せずに終了のみ(本番では systemctl reboot に置き換える)
    state.on_reboot(|| {
        let _ = slint::quit_event_loop();
    });

    ui.run()
}

fn main() {
    // 画面を開けない場合も、CUIには何も表示せずログだけ残す
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::write("/tmp/kaho-installer-panic.log", info.to_string());
    }));
    if let Err(e) = run() {
        let _ = std::fs::write("/tmp/kaho-installer-fatal.log", e.to_string());
        std::process::exit(1);
    }
}
