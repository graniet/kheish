/// SSH module for remote server operations
///
/// This module provides functionality to:
/// - Connect to remote servers via SSH
/// - Execute commands remotely
/// - Transfer files via SCP
/// - Manage SSH sessions and authentication
///
/// The module maintains session state using a global `SSH_SESSION` variable
/// and supports key-based authentication with optional passphrase protection.
///
/// # Examples
///
/// ```no_run
/// // Connect to a remote server
/// ssh.handle_action("connect", &["host=example.com", "user=admin"]);
///
/// // Run a remote command
/// ssh.handle_action("run", &["ls -la"]);
///
/// // Upload a file
/// ssh.handle_action("upload", &["/local/path", "/remote/path"]);
/// ```
use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};
use dialoguer::{Confirm, Password};
use dirs::home_dir;
use once_cell::sync::Lazy;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Represents an SSH session state
#[derive(Debug, Default)]
struct SshSession {
    connected: bool,
    host: Option<String>,
    user: Option<String>,
    key: Option<String>,
    passphrase_required: bool,
    passphrase: Option<String>,
}

/// Global SSH session state
static SSH_SESSION: Lazy<Arc<Mutex<SshSession>>> =
    Lazy::new(|| Arc::new(Mutex::new(SshSession::default())));

/// Main SSH module implementation
pub struct SshModule;

impl std::fmt::Debug for SshModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SshModule")
    }
}

/// Checks if an SSH key requires a passphrase
///
/// # Arguments
/// * `key_path` - Path to the SSH key file
///
/// # Returns
/// `true` if the key requires a passphrase, `false` otherwise
fn check_key_passphrase_needed(key_path: &str) -> bool {
    let output = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(key_path)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output();

    if let Ok(out) = output {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.to_lowercase().contains("enter passphrase") {
            return true;
        }
    }
    false
}

/// Executes an SSH or SCP command with the current session configuration
///
/// # Arguments
/// * `base_cmd` - Base command to run ("ssh" or "scp")
/// * `args` - Command arguments
/// * `session` - Current SSH session state
///
/// # Returns
/// Command output as a string or error message
fn run_ssh_command(base_cmd: &str, args: &[&str], session: &SshSession) -> Result<String, String> {
    let mut cmd = Command::new(base_cmd);
    if let Some(k) = &session.key {
        cmd.arg("-i").arg(k);
    }

    if base_cmd == "ssh" {
        let full_target = format!(
            "{}@{}",
            session.user.as_ref().unwrap(),
            session.host.as_ref().unwrap()
        );
        cmd.arg(full_target);
    }

    for a in args {
        cmd.arg(a);
    }

    if let Some(pass) = &session.passphrase {
        if Command::new("sshpass").arg("-V").output().is_ok() {
            let mut sshpass_cmd = Command::new("sshpass");
            sshpass_cmd.arg("-p").arg(pass).arg(base_cmd);
            if let Some(k) = &session.key {
                sshpass_cmd.arg("-i").arg(k);
            }
            if base_cmd == "ssh" {
                let full_target = format!(
                    "{}@{}",
                    session.user.as_ref().unwrap(),
                    session.host.as_ref().unwrap()
                );
                sshpass_cmd.arg(full_target);
            }
            for a in args {
                sshpass_cmd.arg(a);
            }

            let out = sshpass_cmd.output().map_err(|e| e.to_string())?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);

            if !stderr.trim().is_empty() {
                Ok(format!("STDOUT:\n{}\n\nSTDERR:\n{}", stdout, stderr))
            } else {
                Ok(stdout.trim().to_string())
            }
        } else {
            Err("Key requires a passphrase, but 'sshpass' is not installed. Please install it or provide a key without a passphrase.".into())
        }
    } else {
        let out = cmd.output().map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        if !stderr.trim().is_empty() {
            Ok(format!("STDOUT:\n{}\n\nSTDERR:\n{}", stdout, stderr))
        } else {
            Ok(stdout.trim().to_string())
        }
    }
}

#[async_trait::async_trait]
impl Module for SshModule {
    fn name(&self) -> &str {
        "ssh"
    }

    async fn handle_action(
        &self,
        _vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        match action {
            "connect" => {
                let mut host = None;
                let mut user = None;
                let mut key = None;

                for p in params {
                    if let Some(eq_idx) = p.find('=') {
                        let k = &p[..eq_idx].trim();
                        let v = &p[eq_idx + 1..].trim();
                        match *k {
                            "host" => host = Some(v.to_string()),
                            "user" => user = Some(v.to_string()),
                            "key" => key = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }

                if host.is_none() || user.is_none() {
                    return Err("Missing host or user parameter. Usage: ssh connect host=<host> user=<user> [key=<path>]".into());
                }

                if key.is_none() {
                    if let Some(home) = home_dir() {
                        let default_key_path = home.join(".ssh").join("id_rsa");
                        if default_key_path.exists() {
                            key = Some(default_key_path.to_string_lossy().to_string());
                        }
                    }
                }

                let mut session = SSH_SESSION.lock().unwrap();
                session.connected = false;
                session.host = host;
                session.user = user;
                session.key = key;

                if let Some(k) = &session.key {
                    if check_key_passphrase_needed(k) {
                        session.passphrase_required = true;
                        println!("\nThe chosen key is passphrase-protected.");

                        let confirmed = Confirm::new()
                            .with_prompt("Do you want to provide the passphrase now?")
                            .default(true)
                            .interact()
                            .map_err(|e| e.to_string())?;
                        if confirmed {
                            let pass = Password::new()
                                .with_prompt("Enter your key passphrase")
                                .interact()
                                .map_err(|e| e.to_string())?;
                            session.passphrase = Some(pass);
                        } else {
                            return Err(
                                "Key requires a passphrase and none was provided. Cannot connect."
                                    .into(),
                            );
                        }
                    }
                }

                session.connected = true;
                Ok(
                    "SSH session info stored. Use 'ssh run \"<command>\"' to execute commands."
                        .into(),
                )
            }

            "run" => {
                if params.is_empty() {
                    return Err("Missing command. Usage: ssh run \"<command>\"".into());
                }

                let session = SSH_SESSION.lock().unwrap();
                if !session.connected {
                    return Err("Not connected to any SSH host. Use ssh connect first.".into());
                }

                run_ssh_command("ssh", &[params.join(" ").as_str()], &session)
            }

            "disconnect" => {
                let mut session = SSH_SESSION.lock().unwrap();
                if !session.connected {
                    return Ok("No active SSH session.".into());
                }
                session.connected = false;
                session.host = None;
                session.user = None;
                session.key = None;
                session.passphrase_required = false;
                session.passphrase = None;
                Ok("SSH session disconnected.".into())
            }

            "upload" => {
                if params.len() < 2 {
                    return Err("Usage: ssh upload <local_path> <remote_path>".into());
                }
                let local_path = &params[0];
                let remote_path = &params[1];

                let session = SSH_SESSION.lock().unwrap();
                if !session.connected {
                    return Err("Not connected. Use ssh connect first.".into());
                }

                let host = session.host.as_ref().unwrap();
                let user = session.user.as_ref().unwrap();
                let full_target = format!("{}@{}:{}", user, host, remote_path);

                run_ssh_command("scp", &[local_path, full_target.as_str()], &session)
            }

            "download" => {
                if params.len() < 2 {
                    return Err("Usage: ssh download <remote_path> <local_path>".into());
                }
                let remote_path = &params[0];
                let local_path = &params[1];

                let session = SSH_SESSION.lock().unwrap();
                if !session.connected {
                    return Err("Not connected. Use ssh connect first.".into());
                }

                let host = session.host.as_ref().unwrap();
                let user = session.user.as_ref().unwrap();
                let full_target = format!("{}@{}:{}", user, host, remote_path);

                run_ssh_command("scp", &[full_target.as_str(), local_path], &session)
            }

            "check_connection" => {
                let session = SSH_SESSION.lock().unwrap();
                if session.connected {
                    Ok(format!(
                        "Connected to {}@{}",
                        session.user.as_ref().unwrap(),
                        session.host.as_ref().unwrap()
                    ))
                } else {
                    Ok("Not connected.".into())
                }
            }

            _ => Err(format!("Unknown action '{}'", action)),
        }
    }

    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "connect".to_string(),
                arg_count: 1,
                description: "Connect to a remote server. Usage: ssh connect host=<host> user=<user> [key=<path>]".to_string(),
            },
            ModuleAction {
                name: "run".to_string(),
                arg_count: 1,
                description: "Run a command on the remote server. Usage: ssh run \"<command>\"".to_string(),
            },
            ModuleAction {
                name: "disconnect".to_string(),
                arg_count: 0,
                description: "Disconnect the current SSH session.".to_string(),
            },
            ModuleAction {
                name: "upload".to_string(),
                arg_count: 2,
                description: "Upload a local file. Usage: ssh upload <local_path> <remote_path>".to_string(),
            },
            ModuleAction {
                name: "download".to_string(),
                arg_count: 2,
                description: "Download a file. Usage: ssh download <remote_path> <local_path>".to_string(),
            },
            ModuleAction {
                name: "check_connection".to_string(),
                arg_count: 0,
                description: "Check if currently connected.".to_string(),
            },
        ]
    }
}
