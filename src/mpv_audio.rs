//! Windows mpv output policy and its bounded launch-time capability check.
//!
//! OpenAL Soft supplies a separate WASAPI latency model. This is a launch
//! policy, not a claim that a player's internal A/V clock measures physical
//! speaker/screen synchronization. No global audio or mpv settings are written.
//! The only additional mpv option is stop-on-init-failure: if the selected
//! audio output cannot start, stop instead of playing silent video. It does
//! not touch the synchronization strategy.
use anyhow::{Context, Result, bail, ensure};
use crate::core::Player;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Packaged OpenAL Soft configuration, relative to the launcher executable.
const OPENAL_PROFILE: &str = "assets/audio/openal-wasapi.ini";
/// The packaged profile is a handful of lines; anything larger is not ours.
const MAX_PROFILE_BYTES: usize = 16 * 1024;
const OPENAL_DRIVER: &str = "openal";
/// Header row of the mpv --ao=help table. mpv composes it from its generic
/// "Available %s:" format and the "audio outputs" label, so the constant is
/// spelled out here instead of being inferred from arbitrary help text.
const OPENAL_HELP_HEADER: &str = "Available audio outputs:";

/// Appends the default audio output for the packaged mpv.
///
/// Only the output (and the silent-playback guard) is defaulted. The launcher's
/// existing video/sync options and mpv's own defaults stay exactly as they
/// were: this policy fixes the audio clock source, it does not force a new
/// sync strategy.
pub(crate) fn append_defaults(arguments: &mut Vec<OsString>, extra: &[String]) {
    // If the audio output cannot be initialized, stop instead of showing video
    // without sound. This guard never changes the sync strategy on normal
    // playback and matches the other launcher's fix.
    arguments.push("--stop-playback-on-init-failure=yes".into());
    // A device selector can itself select a driver. An explicit output choice
    // is authoritative for both the command line and the launch policy, so
    // never add the default next to one.
    if extra.iter().any(|argument| {
        argument.starts_with("--ao=") || argument.starts_with("--audio-device=")
    }) {
        return;
    }
    // Defaults are placed before the user's explicit extra arguments, so a
    // later --ao=... still wins.
    arguments.push(format!("--ao={OPENAL_DRIVER}").into());
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OutputPolicy {
    constrain_openal: bool,
    require_openal: bool,
}

/// Interpret only output/track options before the media separator. The complete
/// argument vector is inspected so later explicit overrides remain effective.
fn output_policy(arguments: &[OsString]) -> OutputPolicy {
    let mut driver = None;
    let mut device = None;
    let mut audio = true;
    for argument in arguments {
        if argument == "--" {
            break;
        }
        let Some(argument) = argument.to_str() else {
            continue;
        };
        if let Some(value) = argument.strip_prefix("--ao=") {
            driver = Some(value);
        } else if let Some(value) = argument.strip_prefix("--audio-device=") {
            device = Some(value);
        } else if argument == "--no-audio" {
            audio = false;
        } else if let Some(value) = argument
            .strip_prefix("--audio=")
            .or_else(|| argument.strip_prefix("--aid="))
        {
            audio = value != "no";
        }
    }
    if !audio {
        return OutputPolicy::default();
    }
    if let Some(driver) = driver {
        let drivers: Vec<_> = driver.split(',').collect();
        return OutputPolicy {
            constrain_openal: drivers.contains(&OPENAL_DRIVER),
            // An explicit fallback list is a user override. Only a strictly
            // selected OpenAL driver requires the capability up front.
            require_openal: drivers == [OPENAL_DRIVER],
        };
    }
    let openal_device = device.is_some_and(|value| value.starts_with("openal/"));
    OutputPolicy {
        constrain_openal: openal_device,
        require_openal: openal_device,
    }
}

fn apply_environment(command: &mut Command, policy: &OutputPolicy, profile: &Path) {
    if policy.constrain_openal {
        // No trailing comma: OpenAL must not fall back to DirectSound or another
        // backend. This affects this player child only, never the launcher or OS.
        command.env("ALSOFT_DRIVERS", "wasapi");
        // OpenAL loads this additional file after ordinary user/global config.
        // It selects shared, non-spatial WASAPI without modifying those files.
        command.env("ALSOFT_CONF", profile);
    }
}

fn validate_profile_text(text: &str) -> Result<()> {
    let mut wasapi = false;
    let mut exclusive = None;
    let mut spatial = None;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            wasapi = line[1..line.len() - 1].trim() == "wasapi";
            continue;
        }
        if !wasapi {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let destination = match name.trim() {
            "exclusive-mode" => &mut exclusive,
            "spatial-api" => &mut spatial,
            _ => continue,
        };
        ensure!(
            destination.is_none(),
            "OpenAL 音频配置包含重复选项，请重新完整解压程序包"
        );
        *destination = Some(value.trim() == "false");
    }
    ensure!(
        exclusive == Some(true) && spatial == Some(true),
        "OpenAL 音频配置缺少共享 WASAPI 设置，请重新完整解压程序包"
    );
    Ok(())
}

fn packaged_profile(launcher: &Path) -> Result<PathBuf> {
    let directory = launcher.parent().context("无法定位启动器音频资源目录")?;
    let path = directory.join(OPENAL_PROFILE);
    let file = std::fs::File::open(&path)
        .context("缺少 assets/audio/openal-wasapi.ini，请重新完整解压程序包")?;
    let mut bytes = Vec::new();
    file.take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("无法读取 OpenAL 音频配置")?;
    ensure!(bytes.len() <= MAX_PROFILE_BYTES, "OpenAL 音频配置超过大小限制");
    let text = std::str::from_utf8(&bytes).context("OpenAL 音频配置必须为 UTF-8 文本")?;
    validate_profile_text(text)?;
    Ok(path)
}

/// A real capability claim needs mpv's audio-outputs table header and an
/// indented openal row with a description. A message that merely mentions
/// "openal" (an error line, a truncated capture) must not count as support.
fn help_table_lists_openal(text: &str) -> bool {
    let mut header = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim() == OPENAL_HELP_HEADER {
            header = true;
            continue;
        }
        if !header || line.len() == line.trim_start().len() {
            continue;
        }
        let mut words = line.split_whitespace();
        if words.next() == Some(OPENAL_DRIVER) && words.next().is_some() {
            return true;
        }
    }
    false
}

fn validate_capabilities(success: bool, output: &[u8], truncated: bool) -> Result<()> {
    ensure!(
        !truncated,
        "mpv 音频能力响应超过限制，无法确认 OpenAL 支持"
    );
    let text = String::from_utf8_lossy(output);
    // The help table itself is the capability claim. Some mpv builds exit
    // non-zero after printing diagnostics, so content decides acceptance; a
    // missing table still fails closed below instead of playing silently.
    if help_table_lists_openal(&text) {
        return Ok(());
    }
    ensure!(success, "mpv 音频能力检查失败，请重新选择有效的 mpv.exe");
    ensure!(
        !text.trim().is_empty(),
        "mpv 音频能力检查没有输出，无法确认 OpenAL 支持，请重新选择有效的 mpv.exe"
    );
    ensure!(
        text.lines().any(|line| line.trim() == OPENAL_HELP_HEADER),
        "mpv 音频能力响应没有音频输出列表，无法确认 OpenAL 支持，请重新选择有效的 mpv.exe"
    );
    bail!(
        "此 mpv 不支持 OpenAL，无法启用直播音频时钟修复。请使用带 OpenAL 的 mpv 构建，或在高级参数中显式选择其他 --ao= 输出（该覆盖不使用默认修复路径）"
    )
}

#[cfg(windows)]
fn check_capabilities(executable: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(3);
    const MAX_BYTES: usize = 64 * 1024;

    let mut child = Command::new(executable)
        .args(["--no-config", "--load-scripts=no", "--ao=help"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW; no playback is opened.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动 mpv 音频能力检查，请检查播放器路径")?;
    let stdout = child.stdout.take().context("无法读取 mpv 音频能力响应")?;
    let stderr = child.stderr.take().context("无法读取 mpv 音频能力响应")?;
    let (sender, receiver) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(2);
    // Drain both streams concurrently so a larger help listing cannot fill a
    // pipe and keep the child from exiting. Neither arbitrary stderr nor probe
    // output is ever logged.
    let streams: [(&str, Box<dyn Read + Send>); 2] = [
        ("mpv-audio-stdout", Box::new(stdout)),
        ("mpv-audio-stderr", Box::new(stderr)),
    ];
    for (name, stream) in streams {
        let sender = sender.clone();
        let spawned = thread::Builder::new().name(name.into()).spawn(move || {
            let stream = stream;
            let mut bytes = Vec::new();
            let result = stream
                .take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send(result);
        });
        if let Err(error) = spawned {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("无法读取 mpv 音频能力响应");
        }
    }
    drop(sender);
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("mpv 音频能力检查超时，请检查播放器或重新选择 mpv.exe");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("无法等待 mpv 音频能力检查");
            }
        }
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut failure: Option<anyhow::Error> = None;
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(Ok(bytes)) => {
                if bytes.len() > MAX_BYTES {
                    truncated = true;
                }
                let mut kept = bytes;
                kept.truncate(MAX_BYTES);
                output.extend_from_slice(&kept);
                output.push(b'\n');
            }
            Ok(Err(error)) => failure = Some(error.into()),
            Err(_) => failure = Some(anyhow::anyhow!("mpv 音频能力响应未能在时限内完成")),
        }
    }
    if let Some(error) = failure {
        return Err(error).context("无法读取 mpv 音频能力响应");
    }
    validate_capabilities(status.success(), &output, truncated)
}

/// Applies the mpv audio policy to one spawn. PotPlayer and every non-mpv
/// player keep their previous launch behaviour and environment untouched.
#[cfg(windows)]
pub(crate) fn configure_launch(
    player: Player,
    executable: &Path,
    arguments: &[OsString],
    command: &mut Command,
) -> Result<()> {
    let launcher = std::env::current_exe().context("无法定位启动器程序")?;
    configure_launch_at(player, &launcher, executable, arguments, command, check_capabilities)
}

/// The testable core of configure_launch: the explicit launcher, packaged
/// profile lookup and capability probe keep every branch offline.
fn configure_launch_at<F>(
    player: Player,
    launcher: &Path,
    executable: &Path,
    arguments: &[OsString],
    command: &mut Command,
    probe: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    if player != Player::Mpv {
        return Ok(());
    }
    let policy = output_policy(arguments);
    if !policy.constrain_openal {
        return Ok(());
    }
    // Check the packaged resource before probing so a broken package produces
    // the actionable "re-extract the package" error.
    let profile = packaged_profile(launcher)?;
    if policy.require_openal {
        probe(executable)?;
    }
    apply_environment(command, &policy, &profile);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn launcher(directory: &Path) -> PathBuf {
        directory.join("live-hub-rust.exe")
    }

    fn write_profile(directory: &Path) -> (PathBuf, PathBuf) {
        let launcher = launcher(directory);
        let profile = directory.join(OPENAL_PROFILE);
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        std::fs::write(
            &profile,
            "[wasapi]\nexclusive-mode=false\nspatial-api=false\n",
        )
        .unwrap();
        (launcher, profile)
    }

    fn never_probe(_: &Path) -> Result<()> {
        panic!("capability probe must not run on this path")
    }

    #[test]
    fn defaults_add_only_the_openal_output_before_explicit_user_flags() {
        let mut planned = args(&["--profile=high-quality"]);
        append_defaults(&mut planned, &[]);
        assert_eq!(
            planned,
            [
                "--profile=high-quality",
                "--stop-playback-on-init-failure=yes",
                "--ao=openal"
            ]
        );
        // The audio clock fix must not smuggle in a new video/sync strategy:
        // these flags were deliberately not forced by the 1.0.6 launch plan.
        for unexpected in ["--video-sync=audio", "--framedrop=vo", "--autosync=30"] {
            assert!(!planned.iter().any(|argument| argument == unexpected));
        }
        // An explicit choice suppresses the default completely; the user's own
        // flag is appended later by the plan and therefore wins.
        for extra in [
            "--ao=wasapi",
            "--ao=null",
            "--ao=openal,wasapi",
            "--audio-device=wasapi/synthetic",
        ] {
            let mut with_override = Vec::new();
            append_defaults(&mut with_override, &[extra.to_owned()]);
            assert_eq!(with_override, ["--stop-playback-on-init-failure=yes"], "{extra}");
            assert!(!with_override.iter().any(|argument| argument == "--ao=openal"));
        }
    }

    #[test]
    fn explicit_output_and_device_choices_override_the_default() {
        let mut defaults = args(&["--profile=high-quality"]);
        append_defaults(&mut defaults, &[]);
        assert_eq!(defaults.last().unwrap(), "--ao=openal");
        assert!(
            defaults
                .iter()
                .position(|argument| argument == "--ao=openal")
                .unwrap()
                > defaults
                    .iter()
                    .position(|argument| argument == "--profile=high-quality")
                    .unwrap()
        );
        let cases = [
            ("--ao=wasapi", OutputPolicy::default()),
            ("--ao=null", OutputPolicy::default()),
            ("--audio-device=wasapi/synthetic", OutputPolicy::default()),
            (
                "--ao=openal,wasapi",
                OutputPolicy {
                    constrain_openal: true,
                    require_openal: false,
                },
            ),
        ];
        for (extra, expected) in cases {
            let mut planned = Vec::new();
            append_defaults(&mut planned, &[extra.to_owned()]);
            planned.push(extra.into());
            assert!(!planned.iter().any(|argument| argument == "--ao=openal"));
            assert_eq!(output_policy(&planned), expected, "{extra}");
        }
    }

    #[test]
    fn effective_selection_handles_overrides_fallbacks_and_disabled_audio() {
        assert_eq!(
            output_policy(&args(&["--ao=openal"])),
            OutputPolicy {
                constrain_openal: true,
                require_openal: true,
            }
        );
        assert_eq!(
            output_policy(&args(&["--ao=openal", "--ao=null"])),
            OutputPolicy::default()
        );
        assert_eq!(
            output_policy(&args(&["--audio-device=openal/synthetic"])),
            OutputPolicy {
                constrain_openal: true,
                require_openal: true,
            }
        );
        assert_eq!(
            output_policy(&args(&["--ao=openal,wasapi"])),
            OutputPolicy {
                constrain_openal: true,
                require_openal: false,
            }
        );
        for disabled in ["--no-audio", "--audio=no", "--aid=no"] {
            assert_eq!(
                output_policy(&args(&["--ao=openal", disabled])),
                OutputPolicy::default(),
                "{disabled}"
            );
        }
        // Nothing after the media separator is an option any more.
        assert_eq!(
            output_policy(&args(&["--ao=null", "--", "--ao=openal"])),
            OutputPolicy::default()
        );
        // Re-enabling audio restores the OpenAL requirement.
        assert_eq!(
            output_policy(&args(&["--ao=openal", "--audio=no", "--aid=auto"])),
            OutputPolicy {
                constrain_openal: true,
                require_openal: true,
            }
        );
    }

    #[test]
    fn openal_is_constrained_in_the_child_environment_only() {
        let directory = tempfile::tempdir().unwrap();
        let (launcher, profile) = write_profile(directory.path());
        let parent_drivers = std::env::var_os("ALSOFT_DRIVERS");
        let parent_config = std::env::var_os("ALSOFT_CONF");

        let mut player = Command::new("synthetic-mpv.exe");
        configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=openal"]),
            &mut player,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(player.get_envs().count(), 2);
        assert!(player.get_envs().any(|(key, value)| {
            key == "ALSOFT_DRIVERS" && value == Some(std::ffi::OsStr::new("wasapi"))
        }));
        assert!(
            player
                .get_envs()
                .any(|(key, value)| key == "ALSOFT_CONF" && value == Some(profile.as_os_str()))
        );

        // The launcher's own process environment is never rewritten.
        assert_eq!(std::env::var_os("ALSOFT_DRIVERS"), parent_drivers);
        assert_eq!(std::env::var_os("ALSOFT_CONF"), parent_config);
    }

    #[test]
    fn potplayer_and_non_openal_launches_get_no_audio_environment() {
        let directory = tempfile::tempdir().unwrap();
        // No packaged profile is needed on paths that do not select OpenAL.
        let launcher = launcher(directory.path());

        let mut potplayer = Command::new("synthetic-potplayer.exe");
        configure_launch_at(
            Player::PotPlayer,
            &launcher,
            Path::new("synthetic-potplayer.exe"),
            &args(&["--ao=openal"]),
            &mut potplayer,
            never_probe,
        )
        .unwrap();
        assert_eq!(potplayer.get_envs().count(), 0);

        let mut mpv_wasapi = Command::new("synthetic-mpv.exe");
        configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=wasapi"]),
            &mut mpv_wasapi,
            never_probe,
        )
        .unwrap();
        assert_eq!(mpv_wasapi.get_envs().count(), 0);

        let mut silent = Command::new("synthetic-mpv.exe");
        configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=openal", "--no-audio"]),
            &mut silent,
            never_probe,
        )
        .unwrap();
        assert_eq!(silent.get_envs().count(), 0);
    }

    #[test]
    fn fallback_list_constrains_the_backend_without_requiring_openal() {
        let directory = tempfile::tempdir().unwrap();
        let (launcher, profile) = write_profile(directory.path());
        let mut player = Command::new("synthetic-mpv.exe");
        configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=openal,wasapi"]),
            &mut player,
            never_probe,
        )
        .unwrap();
        assert_eq!(player.get_envs().count(), 2);
        assert!(
            player
                .get_envs()
                .any(|(key, value)| key == "ALSOFT_CONF" && value == Some(profile.as_os_str()))
        );
    }

    #[test]
    fn capability_failure_never_opens_a_silent_playback_path() {
        let directory = tempfile::tempdir().unwrap();
        let (launcher, _) = write_profile(directory.path());
        let mut player = Command::new("synthetic-mpv.exe");
        let error = configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=openal"]),
            &mut player,
            |_| Err(anyhow::anyhow!("此 mpv 不支持 OpenAL")),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("OpenAL"), "{error:#}");
        // The failed preflight must not leave a partially configured child.
        assert_eq!(player.get_envs().count(), 0);
    }

    #[test]
    fn missing_profile_is_rejected_before_the_capability_probe() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = launcher(directory.path());
        let mut player = Command::new("synthetic-mpv.exe");
        let error = configure_launch_at(
            Player::Mpv,
            &launcher,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=openal"]),
            &mut player,
            never_probe,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("assets/audio/openal-wasapi.ini"),
            "{error:#}"
        );
        assert_eq!(player.get_envs().count(), 0);
    }

    #[test]
    fn bundled_audio_profile_is_required_and_validated_offline() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = launcher(directory.path());
        assert!(packaged_profile(&launcher).is_err());
        let (_, profile) = write_profile(directory.path());
        assert_eq!(packaged_profile(&launcher).unwrap(), profile);
        // A BOM, comments, blank lines and CRLF are all accepted.
        std::fs::write(
            &profile,
            "\u{feff}[wasapi]\r\n# shared output\r\n\r\nexclusive-mode=false\r\nspatial-api=false\r\n",
        )
        .unwrap();
        assert!(packaged_profile(&launcher).is_ok());
        for invalid in [
            "[wasapi]\nexclusive-mode=true\nspatial-api=false\n",
            "[wasapi]\nexclusive-mode=false\n",
            "[general]\nexclusive-mode=false\nspatial-api=false\n",
            "[wasapi]\nexclusive-mode=false\nexclusive-mode=false\nspatial-api=false\n",
            "[wasapi]\nspatial-api=false\nexclusive-mode=false\nexclusive-mode=false\n",
            "not a config",
        ] {
            std::fs::write(&profile, invalid).unwrap();
            assert!(packaged_profile(&launcher).is_err(), "{invalid}");
        }
        std::fs::write(&profile, vec![b'x'; MAX_PROFILE_BYTES + 1]).unwrap();
        assert!(packaged_profile(&launcher).is_err());
    }

    #[test]
    fn capability_fixtures_require_a_real_help_table_row() {
        // The exact row shape printed by the packaged mpv build:
        //   Available audio outputs:
        //     openal          OpenAL audio output
        let supported = b"Available audio outputs:\r\n  wasapi          Windows Audio Session API\r\n  openal          OpenAL audio output\r\n  null            Null audio output\r\n";
        assert!(validate_capabilities(true, supported, false).is_ok());
        // Some builds report a non-zero status after printing the table; the
        // table content still decides, so a usable player is not rejected.
        assert!(validate_capabilities(false, supported, false).is_ok());

        let rejected: [&[u8]; 7] = [
            // Header only, no driver row.
            b"Available audio outputs:\r\n",
            // Mentioning openal is not a capability claim.
            b"Error opening openal output\n",
            b"openal is unavailable\n",
            // A row without the help table header.
            b"  openal          OpenAL audio output\n",
            // Header without an openal row.
            b"Available audio outputs:\n  wasapi Windows WASAPI\n  null Null output\n",
            // Empty capture.
            b"",
            // A mentioned option in an error message is not a help row.
            b"Available audio outputs:\nError: --ao=openal cannot be used\n",
        ];
        for fixture in rejected {
            assert!(
                validate_capabilities(true, fixture, false).is_err(),
                "{}",
                String::from_utf8_lossy(fixture)
            );
        }
        let missing = validate_capabilities(
            true,
            b"Available audio outputs:\n  wasapi Windows WASAPI\n  null Null output\n",
            false,
        )
        .unwrap_err();
        assert!(missing.to_string().contains("OpenAL"), "{missing:#}");
        assert!(missing.to_string().contains("--ao="), "{missing:#}");
        assert!(validate_capabilities(true, supported, true).is_err());
        assert!(validate_capabilities(false, b"", false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn capability_probe_with_a_missing_player_is_actionable_and_never_spawns_playback() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-mpv.exe");
        let error = check_capabilities(&missing).unwrap_err();
        assert!(format!("{error:#}").contains("mpv"), "{error:#}");
    }

    #[cfg(windows)]
    #[test]
    fn launch_policy_guards_potplayer_and_non_openal_paths() {
        // The production wrapper must not require the packaged profile (or the
        // real probe) for PotPlayer and for explicitly overridden outputs.
        let mut potplayer = Command::new("synthetic-potplayer.exe");
        configure_launch(
            Player::PotPlayer,
            Path::new("synthetic-potplayer.exe"),
            &args(&["--ao=openal"]),
            &mut potplayer,
        )
        .unwrap();
        assert_eq!(potplayer.get_envs().count(), 0);

        let mut mpv_override = Command::new("synthetic-mpv.exe");
        configure_launch(
            Player::Mpv,
            Path::new("synthetic-mpv.exe"),
            &args(&["--ao=null"]),
            &mut mpv_override,
        )
        .unwrap();
        assert_eq!(mpv_override.get_envs().count(), 0);
    }
}
