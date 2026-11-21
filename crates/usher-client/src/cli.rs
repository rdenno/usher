use anyhow::{Result, bail};
use std::collections::HashSet;

const SSH_BOOL_FLAGS: &str = "46AaCKkMqnTXxYyv";
const SSH_VALUE_FLAGS: &str = "BbcDEeFIiJLlmoOpQRSwW";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitCli {
    pub usher_args: Vec<String>,
    pub ssh_args: Vec<String>,
    pub remote_cmd: Vec<String>,
}

/// Splits raw CLI arguments into Usher flags/positional target, OpenSSH passthrough flags,
/// and any trailing ephemeral session command after `--`.
pub fn split_cli_args(raw_args: Vec<String>, usher_cmd: &clap::Command) -> Result<SplitCli> {
    let mut long_value_flags = HashSet::new();
    for arg in usher_cmd.get_arguments() {
        if let Some(long) = arg.get_long()
            && arg.get_action().takes_values()
        {
            long_value_flags.insert(long.to_string());
        }
    }

    let mut usher_args = Vec::new();
    let mut ssh_args = Vec::new();
    let mut remote_cmd = Vec::new();

    if let Some(prog) = raw_args.first() {
        usher_args.push(prog.clone());
    }

    let mut i = 1usize;
    while i < raw_args.len() {
        let tok = &raw_args[i];
        if tok == "--" {
            remote_cmd.extend(raw_args[i + 1..].iter().cloned());
            break;
        }
        if let Some(body) = tok.strip_prefix("--") {
            usher_args.push(tok.clone());
            if !body.contains('=') && long_value_flags.contains(body) {
                i += 1;
                if i < raw_args.len() {
                    usher_args.push(raw_args[i].clone());
                }
            }
            i += 1;
            continue;
        }
        if tok == "-h" || tok == "-V" {
            usher_args.push(tok.clone());
            i += 1;
            continue;
        }
        if let Some(chars) = tok.strip_prefix('-')
            && !chars.is_empty()
        {
            let mut handled_ssh_value = false;
            let mut all_ssh_bool = true;

            for (byte_idx, ch) in chars.char_indices() {
                if SSH_BOOL_FLAGS.contains(ch) {
                    continue;
                }
                if SSH_VALUE_FLAGS.contains(ch) {
                    ssh_args.push(tok.clone());
                    let after = &chars[byte_idx + ch.len_utf8()..];
                    if after.is_empty() {
                        i += 1;
                        if i >= raw_args.len() {
                            bail!("SSH option {tok} requires an argument");
                        }
                        ssh_args.push(raw_args[i].clone());
                    }
                    handled_ssh_value = true;
                    all_ssh_bool = false;
                    break;
                }
                all_ssh_bool = false;
                break;
            }

            if handled_ssh_value || all_ssh_bool {
                if !handled_ssh_value {
                    ssh_args.push(tok.clone());
                }
                i += 1;
                continue;
            }

            // Unknown short option -> let clap report a clean Usher usage error.
            usher_args.push(tok.clone());
            i += 1;
            continue;
        }

        usher_args.push(tok.clone());
        i += 1;
    }

    Ok(SplitCli {
        usher_args,
        ssh_args,
        remote_cmd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    fn test_cmd() -> Command {
        Command::new("usher")
            .arg(Arg::new("host").index(1))
            .arg(Arg::new("local").long("local").action(ArgAction::SetTrue))
            .arg(Arg::new("doctor").long("doctor").action(ArgAction::SetTrue))
            .arg(
                Arg::new("codec")
                    .long("codec")
                    .action(ArgAction::Set),
            )
            .arg(
                Arg::new("transport")
                    .long("transport")
                    .action(ArgAction::Set),
            )
            .arg(
                Arg::new("resolution")
                    .long("resolution")
                    .action(ArgAction::Set),
            )
    }

    #[test]
    fn parses_openssh_flags_proxyjump_and_remote_cmd() {
        let cmd = test_cmd();
        let raw = vec![
            "usher".into(),
            "-J".into(),
            "bastion".into(),
            "-p".into(),
            "2222".into(),
            "-i".into(),
            "/home/me/.ssh/id_ed25519".into(),
            "-o".into(),
            "ControlMaster=auto".into(),
            "-A".into(),
            "--codec".into(),
            "hevc".into(),
            "my-server".into(),
            "--".into(),
            "firefox".into(),
            "--new-window".into(),
        ];
        let split = split_cli_args(raw, &cmd).unwrap();
        assert_eq!(
            split.ssh_args,
            vec![
                "-J",
                "bastion",
                "-p",
                "2222",
                "-i",
                "/home/me/.ssh/id_ed25519",
                "-o",
                "ControlMaster=auto",
                "-A",
            ]
        );
        assert_eq!(split.usher_args, vec!["usher", "--codec", "hevc", "my-server"]);
        assert_eq!(split.remote_cmd, vec!["firefox", "--new-window"]);
    }

    #[test]
    fn handles_attached_and_clustered_ssh_flags() {
        let cmd = test_cmd();
        let raw = vec![
            "usher".into(),
            "-4AT".into(),
            "-p2200".into(),
            "-4i/tmp/key".into(),
            "-oBatchMode=no".into(),
            "box".into(),
        ];
        let split = split_cli_args(raw, &cmd).unwrap();
        assert_eq!(
            split.ssh_args,
            vec!["-4AT", "-p2200", "-4i/tmp/key", "-oBatchMode=no"]
        );
        assert_eq!(split.usher_args, vec!["usher", "box"]);
        assert!(split.remote_cmd.is_empty());

        let missing = split_cli_args(vec!["usher".into(), "-p".into()], &cmd);
        assert!(missing.is_err());
    }

    #[test]
    fn preserves_usher_flags_and_doctor_with_ssh_args() {
        let cmd = test_cmd();
        let raw = vec![
            "usher".into(),
            "--doctor".into(),
            "-F".into(),
            "/tmp/ssh_cfg".into(),
            "--transport=tcp".into(),
            "lolas-computer".into(),
        ];
        let split = split_cli_args(raw, &cmd).unwrap();
        assert_eq!(split.ssh_args, vec!["-F", "/tmp/ssh_cfg"]);
        assert_eq!(
            split.usher_args,
            vec!["usher", "--doctor", "--transport=tcp", "lolas-computer"]
        );
    }

    #[test]
    fn preserves_double_dash_command_with_hyphens() {
        let cmd = test_cmd();
        let raw = vec![
            "usher".into(),
            "--local".into(),
            "--".into(),
            "mpv".into(),
            "-fs".into(),
            "--no-audio".into(),
            "-p".into(),
            "foo".into(),
        ];
        let split = split_cli_args(raw, &cmd).unwrap();
        assert!(split.ssh_args.is_empty());
        assert_eq!(split.usher_args, vec!["usher", "--local"]);
        assert_eq!(
            split.remote_cmd,
            vec!["mpv", "-fs", "--no-audio", "-p", "foo"]
        );
    }
}
