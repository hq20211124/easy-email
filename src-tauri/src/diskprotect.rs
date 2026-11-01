//! Does this machine actually encrypt the disk the cache is written to?
//!
//! `design/A3-local-store-protection.md` 选项 2: no mainstream desktop client treats its local store
//! as the security boundary (A3-F1/F5/F10/F11/F15) — the boundary is the **volume**, which is why the
//! deployment guide makes BitLocker a precondition instead of shipping an encrypted database. A
//! precondition nobody can check is a sentence in a PDF, so the app asks the operating system once
//! per start and says what it found.
//!
//! The question needs administrator rights. Measured on Windows 10.0.26300, a normal process gets
//! *Access Denied* from both `manage-bde -status` and the CIM class used here. So `Unknown` is a real
//! answer and not a failure: the report says which states it can and cannot distinguish, and what
//! would resolve it ([`report::NEEDS_ADMIN`]). It never guesses toward "protected".
//!
//! The numbers, not the words: `manage-bde` and the CIM *description* fields come back in the OS
//! display language, so any parser over them breaks on a Chinese Windows. `ConversionStatus` and
//! `ProtectionStatus` are documented integers, and the script below prints exactly those.

use serde::Serialize;

/// What one volume's numbers mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeState {
    /// Fully encrypted and protection is on.
    Protected,
    /// The data on the volume is encrypted while `ProtectionStatus` says the protector is **off**
    /// (0): the keys sit on the volume itself. Not the same as `Protected`, and it must not be
    /// reported as it — this is the state in which a reboot into recovery finds the volume readable.
    /// A protection status of *unknown* (2) is not folded in here; it stays `Unknown`, because
    /// "we cannot tell" and "we can tell it is off" are different findings.
    EncryptedUnprotected,
    /// The volume is not encrypted.
    NotEncrypted,
    /// Encryption or decryption is running, or was asked for and has not finished.
    Converting,
    /// A number this code does not know. Kept distinct from every real state, because the honest
    /// reading of an undocumented value is "I do not know", not "probably fine".
    Unknown,
}

/// One fixed volume the query named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Volume {
    pub drive: String,
    pub state: VolumeState,
    /// The raw `ConversionStatus`, or `None` when the line did not carry one. Kept in the report so a
    /// future value can be read from the log instead of guessed from the code that was running.
    pub conversion: Option<i64>,
    pub protection: Option<i64>,
}

/// The one-word answer for the whole machine, derived from every volume the query saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every volume the query saw is encrypted with protection on.
    Protected,
    /// No volume it saw is encrypted.
    NotEncrypted,
    /// Some are, some are not — which is a finding, not a rounding error.
    Mixed,
    /// At least one volume is mid-conversion, so no verdict is stable yet.
    Converting,
    /// At least one volume is encrypted but its protection is not confirmed on.
    EncryptedUnprotected,
    /// Nothing could be determined. `note` says why.
    Unknown,
}

/// What the app tells the window, and writes to the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiskProtectionReport {
    pub volumes: Vec<Volume>,
    pub verdict: Verdict,
    /// One sentence, in the language the panel already uses.
    pub note: String,
}

/// The sentence for the state a normal process cannot resolve.
pub const NEEDS_ADMIN: &str = "无法判定：BitLocker 状态查询需要管理员权限（以管理员身份运行本程序，或让 IT 运行 \
     tools/disk-protection-check.ps1 并留存输出）。";

/// The sentence when the query ran but the machine is not encrypted.
pub const NOT_ENCRYPTED: &str = "系统盘未加密：本地缓存文件对能拿到这台机器的人是完全明文的。部署前置条件要求先开启 BitLocker \
    （或与等保一致的国产全盘加密），见 design/A3-local-store-protection.md。";

impl DiskProtectionReport {
    /// Assemble the answer from whatever the query returned.
    ///
    /// The note is derived here rather than at the call site: the panel prints this string verbatim,
    /// and a note that disagreed with the verdict would be the bug a reviewer cannot reproduce.
    pub fn build(volumes: Vec<Volume>) -> DiskProtectionReport {
        let verdict = verdict_of(&volumes);
        let note = match verdict {
            Verdict::Protected => format!(
                "系统盘已加密且保护已开启（{}）。",
                volumes
                    .iter()
                    .map(|v| v.drive.clone())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Verdict::NotEncrypted => NOT_ENCRYPTED.to_string(),
            Verdict::Mixed => {
                "各卷的状态不一致：只要有一块不满足「已加密且保护已开启」，这台机器就不算满足部署前置条件。\
                 逐卷结果见下。"
                    .to_string()
            }
            Verdict::Converting => "加密或解密仍在进行中，现在的结果不代表最终状态。".to_string(),
            Verdict::EncryptedUnprotected => {
                "磁盘内容已加密，但保护器处于关闭状态：密钥就在这块盘上，重启到恢复环境即可读到。".to_string()
            }
            Verdict::Unknown => NEEDS_ADMIN.to_string(),
        };
        DiskProtectionReport {
            volumes,
            verdict,
            note,
        }
    }

    /// The report shown before the background query has answered.
    pub fn pending() -> DiskProtectionReport {
        DiskProtectionReport {
            volumes: Vec::new(),
            verdict: Verdict::Unknown,
            note: "正在查询本机磁盘加密状态…（这条只读，不改动任何东西）".to_string(),
        }
    }

    /// A report that says the check does not exist on this platform.
    #[cfg(not(windows))]
    pub fn unsupported() -> DiskProtectionReport {
        DiskProtectionReport {
            volumes: Vec::new(),
            verdict: Verdict::Unknown,
            note: "这个自检只在 Windows 上查询卷的加密状态。".to_string(),
        }
    }
}

fn verdict_of(volumes: &[Volume]) -> Verdict {
    if volumes.is_empty() {
        return Verdict::Unknown;
    }
    // One pass, five sets. `Unknown` wins over everything: if a number could not be read, the whole
    // machine is not known to be encrypted, and the report must not say otherwise.
    if volumes.iter().any(|v| v.state == VolumeState::Unknown) {
        return Verdict::Unknown;
    }
    let all = |want: VolumeState| volumes.iter().all(|v| v.state == want);
    if all(VolumeState::Protected) {
        return Verdict::Protected;
    }
    if all(VolumeState::NotEncrypted) {
        return Verdict::NotEncrypted;
    }
    if all(VolumeState::Converting) {
        return Verdict::Converting;
    }
    if all(VolumeState::EncryptedUnprotected) {
        return Verdict::EncryptedUnprotected;
    }
    // Any volume that is not encrypted makes the machine not protected, and the per-volume list says
    // which one; `Mixed` is the word for "the precondition holds on C: and not on D:".
    Verdict::Mixed
}

/// `Win32_EncryptableVolume`'s two documented integers, as the state the panel shows.
///
/// The values are the documented ones: `ConversionStatus` 0 = fully decrypted, 1 = fully encrypted,
/// 2/3/4 = pending or in progress; `ProtectionStatus` 0 = off, 1 = on, 2 = unknown. The pair is
/// matched **together** on purpose: `conversion == 1` by itself is not enough to call a volume
/// protected, so a protection value this code has not seen becomes `Unknown` rather than one of the
/// states a person will read as fact. A conversion still in progress needs no protector to be known
/// — it is not finished whatever the protection field says.
pub fn state_of(conversion: i64, protection: i64) -> VolumeState {
    match (conversion, protection) {
        (1, 1) => VolumeState::Protected,
        // Fully encrypted, protector explicitly off: the keys are on the volume.
        (1, 0) => VolumeState::EncryptedUnprotected,
        (0, _) => VolumeState::NotEncrypted,
        (2..=4, _) => VolumeState::Converting,
        // Includes `(1, 2)` — "protection status unknown". Not the same question as "off".
        _ => VolumeState::Unknown,
    }
}

/// Read the query's output: `C:|1|1` per line, nothing else.
///
/// Lines that do not have three fields with two integers are skipped rather than turned into a
/// verdict, and the first field has to actually look like a drive: a PowerShell error message that
/// reached stdout, or a row with an empty or odd drive, is not evidence about anybody's disk.
pub fn parse_volumes(text: &str) -> Vec<Volume> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim().split('|').collect();
            if parts.len() != 3 {
                return None;
            }
            let drive = parts[0].trim();
            if !looks_like_drive(drive) {
                return None;
            }
            let conversion = parts[1].trim().parse::<i64>().ok()?;
            let protection = parts[2].trim().parse::<i64>().ok()?;
            Some(Volume {
                drive: drive.to_string(),
                state: state_of(conversion, protection),
                conversion: Some(conversion),
                protection: Some(protection),
            })
        })
        .collect()
}

/// `Win32_EncryptableVolume.DriveLetter` is one letter and a colon.
///
/// Checked rather than trusted because the caller is a text stream: accepting `:` or `Everything` as
/// a drive name would let a stray line become a volume in a security report.
fn looks_like_drive(drive: &str) -> bool {
    let bytes = drive.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphanumeric() && bytes[1] == b':'
}

/// The one line of PowerShell this app runs, as a constant.
///
/// Nothing user-supplied reaches it — that is the whole reason launching a shell command from a mail
/// client is defensible. The output format is chosen here so the parser has no locale to trip over.
#[cfg(windows)]
pub const QUERY: &str = r#"Get-CimInstance -Namespace root/CIMV2/Security/MicrosoftVolumeEncryption -ClassName Win32_EncryptableVolume | ForEach-Object { "{0}|{1}|{2}" -f $_.DriveLetter, $_.ConversionStatus, $_.ProtectionStatus }"#;

/// Ask the operating system. Blocks, so it belongs on a thread of its own.
///
/// It is read-only by construction (`Get-CimInstance` of a status class), and every way it can fail —
/// no PowerShell, access denied, WMI unavailable, output we cannot read — ends in `Unknown` with a
/// note. Nothing here changes a protector, writes a key, or touches a volume.
///
/// **`CREATE_NO_WINDOW` is not an optimisation, it is the difference between "an app" and "an app
/// that flashes a cmd window on every start".** `powershell.exe` is a *console* subsystem program:
/// when a GUI process starts one without this flag, Windows allocates a console for it, and the user
/// sees a black window appear behind the app for as long as the query runs — every single launch.
/// This app is built with `windows_subsystem = "windows"` (`main.rs`), which is exactly why the
/// allocation is not automatic and has to be suppressed here.
#[cfg(windows)]
pub fn probe() -> DiskProtectionReport {
    use std::os::windows::process::CommandExt;

    // winbase.h. `CREATE_NO_WINDOW` = 0x08000000: run the child with a console that is never shown.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let text = match std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", QUERY])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let volumes = parse_volumes(&stdout);
            if !volumes.is_empty() {
                return DiskProtectionReport::build(volumes);
            }
            // Empty with a nonzero exit is what access denied looks like; empty with a zero exit is
            // what a machine with no fixed volumes looks like. Both are "not known", and the note has
            // to say so without claiming a reason we did not observe.
            if !out.status.success() {
                return DiskProtectionReport {
                    volumes: Vec::new(),
                    verdict: Verdict::Unknown,
                    note: format!("{NEEDS_ADMIN}（查询退出码 {:?}）", out.status.code()),
                };
            }
            stdout
        }
        Err(e) => {
            return DiskProtectionReport {
                volumes: Vec::new(),
                verdict: Verdict::Unknown,
                note: format!("{NEEDS_ADMIN}（无法启动查询：{e}）"),
            };
        }
    };
    DiskProtectionReport::build(parse_volumes(&text))
}

/// There is nothing to ask on any other platform.
#[cfg(not(windows))]
pub fn probe() -> DiskProtectionReport {
    DiskProtectionReport::unsupported()
}

#[cfg(test)]
mod tests {
    use super::{DiskProtectionReport, NEEDS_ADMIN, Verdict, VolumeState, parse_volumes, state_of};

    #[test]
    fn the_documented_numbers_map_to_the_documented_states() {
        assert_eq!(state_of(1, 1), VolumeState::Protected);
        assert_eq!(state_of(0, 0), VolumeState::NotEncrypted);
        // Encrypted content with the protector **off**: **not** the same answer as protected, and the
        // difference is the one a reviewer asks about (the keys are on the volume itself).
        assert_eq!(state_of(1, 0), VolumeState::EncryptedUnprotected);
        assert_eq!(state_of(3, 0), VolumeState::Converting);
        assert_eq!(state_of(2, 0), VolumeState::Converting);
        assert_eq!(state_of(4, 0), VolumeState::Converting);
        // `ProtectionStatus` 2 is documented as *unknown*, which is a different finding from "off":
        // it must not be reported as the named, actionable state.
        assert_eq!(state_of(1, 2), VolumeState::Unknown);
        // And a number this code has never seen stays unknown rather than becoming good news.
        assert_eq!(state_of(9, 1), VolumeState::Unknown);
        assert_eq!(state_of(1, 9), VolumeState::Unknown);
    }

    #[test]
    fn only_lines_of_three_fields_are_volumes() {
        let volumes = parse_volumes("C:|1|1\nD:|0|0\n");
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].state, VolumeState::Protected);
        assert_eq!(volumes[1].state, VolumeState::NotEncrypted);

        assert!(parse_volumes("").is_empty());
        assert!(parse_volumes("\n  \n").is_empty());
        // An error message that reached stdout, a truncated row, a non-numeric field, and a drive
        // field that is not a drive: none of them is evidence about anybody's disk.
        assert_eq!(
            parse_volumes("Get-CimInstance : 拒绝访问\nC:|1|\nC:|x|1\n:|1|1\nEverything|1|1\n")
                .len(),
            0
        );
        // The same junk with one real row in it: the row survives, the junk does not become a volume.
        let mixed = parse_volumes("拒绝访问|1|1\nC:|1|1\n");
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].drive, "C:");
    }

    #[test]
    fn a_mixed_machine_is_not_reported_as_protected() {
        let report = DiskProtectionReport::build(parse_volumes("C:|1|1\nD:|0|0\n"));
        assert_eq!(report.verdict, Verdict::Mixed);
        assert_eq!(report.volumes.len(), 2);

        // The order of the two lines cannot change the answer, and one encrypted system volume with
        // an unencrypted data volume is exactly the deployment that would otherwise read as compliant.
        let flipped = DiskProtectionReport::build(parse_volumes("D:|0|0\nC:|1|1\n"));
        assert_eq!(flipped.verdict, Verdict::Mixed);

        let good = DiskProtectionReport::build(parse_volumes("C:|1|1\nD:|1|1\n"));
        assert_eq!(good.verdict, Verdict::Protected);
        assert!(good.note.contains("C:"));

        let bad = DiskProtectionReport::build(parse_volumes("C:|0|0\n"));
        assert_eq!(bad.verdict, Verdict::NotEncrypted);
        assert!(bad.note.contains("未加密"));

        let empty = DiskProtectionReport::build(Vec::new());
        assert_eq!(empty.verdict, Verdict::Unknown);
        assert_eq!(empty.note, NEEDS_ADMIN);
    }

    /// **A volume whose protector is off must not lift the machine's verdict to "protected".**
    ///
    /// The all-off case has its own verdict, one protected plus one off is `Mixed`, and one protected
    /// plus one *unknown number* is `Unknown` — the report never says the good word when any part of
    /// the evidence is missing or contradicts it.
    #[test]
    fn protection_off_is_never_reported_as_protected() {
        assert_eq!(
            DiskProtectionReport::build(parse_volumes("C:|1|0\n")).verdict,
            Verdict::EncryptedUnprotected
        );
        assert_eq!(
            DiskProtectionReport::build(parse_volumes("C:|1|1\nD:|1|0\n")).verdict,
            Verdict::Mixed
        );
        assert_eq!(
            DiskProtectionReport::build(parse_volumes("C:|1|1\nD:|9|9\n")).verdict,
            Verdict::Unknown
        );
        // Halfway through an operation the answer is not stable, and the mixed case still refuses the
        // good word: a reviewer reading only the headline must never be told "protected" by it.
        assert_eq!(
            DiskProtectionReport::build(parse_volumes("C:|1|1\nD:|3|0\n")).verdict,
            Verdict::Mixed
        );
        assert_eq!(
            DiskProtectionReport::build(parse_volumes("C:|3|0\n")).verdict,
            Verdict::Converting
        );
    }
}
