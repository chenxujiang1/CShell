use super::{MAX_TASK_COMMAND_BYTES, TaskError};
use cshell_domain::*;

fn identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && value.len() <= 64
}
fn cmd_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b" _-./\\:,='".contains(&b))
}
fn quote(dialect: TaskShellDialect, value: &str) -> Result<String, TaskError> {
    if value.contains('\0') {
        return Err(TaskError::Invalid);
    }
    Ok(match dialect {
        TaskShellDialect::Posix => format!("'{}'", value.replace('\'', "'\\''")),
        TaskShellDialect::PowerShell => format!("'{}'", value.replace('\'', "''")),
        TaskShellDialect::Cmd if cmd_value(value) => format!("\"{value}\""),
        TaskShellDialect::Cmd => return Err(TaskError::UnsafeCmdValue),
    })
}
fn encoded_powershell(value: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<_> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(ALPHABET[((bits >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((bits >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}
fn powershell_launcher(script: &str) -> String {
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        encoded_powershell(&format!(
            "$ProgressPreference = 'SilentlyContinue'\n{script}"
        ))
    )
}
fn cmd_with_environment(script: &str, definition: &TaskDefinition) -> Result<String, TaskError> {
    // Process environment is set before Cmd expands %NAME%. `set ... && body`
    // on one command line would expand the body using the old environment.
    let mut launch = format!(
        "$ErrorActionPreference='Stop'\n$p=New-Object System.Diagnostics.Process\n$p.StartInfo.FileName='cmd.exe'\n$p.StartInfo.Arguments='/D /V:OFF /S /C \"' + {} + '\"'\n$p.StartInfo.UseShellExecute=$false\n$p.StartInfo.CreateNoWindow=$true\n$p.StartInfo.RedirectStandardOutput=$true\n$p.StartInfo.RedirectStandardError=$true\n",
        quote(TaskShellDialect::PowerShell, script)?
    );
    for (key, value) in &definition.environment {
        if !cmd_value(value) {
            return Err(TaskError::UnsafeCmdValue);
        }
        launch.push_str(&format!(
            "$p.StartInfo.EnvironmentVariables[{}]={}\n",
            quote(TaskShellDialect::PowerShell, key)?,
            quote(TaskShellDialect::PowerShell, value)?
        ));
    }
    launch.push_str("try { if (-not $p.Start()) { exit 1 }; $out=$p.StandardOutput.BaseStream.CopyToAsync([Console]::OpenStandardOutput()); $err=$p.StandardError.BaseStream.CopyToAsync([Console]::OpenStandardError()); $p.WaitForExit(); [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]@($out,$err)); exit $p.ExitCode } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 } finally { $p.Dispose() }");
    Ok(powershell_launcher(&launch))
}
pub(super) fn compile(definition: &TaskDefinition) -> Result<Vec<String>, TaskError> {
    if definition.variables.len() > 64 || definition.environment.len() > 64 {
        return Err(TaskError::Limit);
    }
    for (key, value) in definition.variables.iter().chain(&definition.environment) {
        if !identifier(key) || value.len() > 8192 || value.contains('\0') {
            return Err(TaskError::Invalid);
        }
    }
    let dialect = match &definition.shell {
        RemoteShellProfile::Posix => TaskShellDialect::Posix,
        RemoteShellProfile::PowerShell => TaskShellDialect::PowerShell,
        RemoteShellProfile::Cmd => TaskShellDialect::Cmd,
        RemoteShellProfile::Custom { dialect, launcher } => {
            if launcher.len() > 4096
                || launcher.chars().any(char::is_control)
                || launcher.matches("{script}").count() != 1
                || launcher.trim() == "{script}"
            {
                return Err(TaskError::Invalid);
            }
            *dialect
        }
    };
    let mut result = Vec::new();
    let mut total = 0;
    for step in &definition.steps {
        let mut script = String::new();
        for (key, value) in &definition.environment {
            match dialect {
                TaskShellDialect::Posix => {
                    script.push_str(&format!("export {key}={}\n", quote(dialect, value)?))
                }
                TaskShellDialect::PowerShell => {
                    script.push_str(&format!("$env:{key} = {}\n", quote(dialect, value)?))
                }
                TaskShellDialect::Cmd if cmd_value(value) => {}
                TaskShellDialect::Cmd => return Err(TaskError::UnsafeCmdValue),
            }
        }
        match &step.command {
            TaskCommand::Script { text } => {
                if text.contains('\0') {
                    return Err(TaskError::Invalid);
                }
                script.push_str(text);
            }
            TaskCommand::Invocation { program, arguments } => {
                if program.trim().is_empty()
                    || program.len() > 4096
                    || program.chars().any(char::is_control)
                {
                    return Err(TaskError::Invalid);
                }
                if arguments.len() > 256 {
                    return Err(TaskError::Limit);
                }
                if dialect == TaskShellDialect::PowerShell {
                    script.push_str("& ");
                }
                script.push_str(&quote(dialect, program)?);
                for argument in arguments {
                    let value = match argument {
                        TaskArgument::Literal { text } => text,
                        TaskArgument::Variable { name } => definition
                            .variables
                            .get(name)
                            .ok_or(TaskError::MissingVariable)?,
                    };
                    if value.len() > 8192 {
                        return Err(TaskError::Limit);
                    }
                    script.push(' ');
                    script.push_str(&quote(dialect, value)?);
                }
            }
        }
        if script.len() > MAX_TASK_COMMAND_BYTES {
            return Err(TaskError::Limit);
        }
        if script.trim().is_empty() {
            return Err(TaskError::Invalid);
        }
        let command = match &definition.shell {
            RemoteShellProfile::Posix => format!(
                "exec /bin/sh -c {}",
                quote(TaskShellDialect::Posix, &script)?
            ),
            RemoteShellProfile::PowerShell => {
                let script = format!(
                    "$ErrorActionPreference = 'Stop'\ntry {{ & {{\n{script}\n}}; if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 0 }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}"
                );
                powershell_launcher(&script)
            }
            RemoteShellProfile::Cmd
                if !script.contains(['\r', '\n']) && !definition.environment.is_empty() =>
            {
                cmd_with_environment(&script, definition)?
            }
            RemoteShellProfile::Cmd if !script.contains(['\r', '\n']) => {
                format!("cmd.exe /D /V:OFF /S /C \"{script}\"")
            }
            RemoteShellProfile::Cmd => return Err(TaskError::UnsafeCmdValue),
            RemoteShellProfile::Custom { launcher, .. } => {
                let command = launcher.replace("{script}", &quote(dialect, &script)?);
                if dialect == TaskShellDialect::Cmd && !definition.environment.is_empty() {
                    cmd_with_environment(&command, definition)?
                } else {
                    command
                }
            }
        };
        total += command.len();
        if command.len() > MAX_TASK_COMMAND_BYTES * 8 || total > 2 * 1024 * 1024 {
            return Err(TaskError::Limit);
        }
        result.push(command);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialects_have_distinct_quoting_and_powershell_is_utf16_base64() -> Result<(), TaskError> {
        assert_eq!(quote(TaskShellDialect::Posix, "a'b;$x")?, "'a'\\''b;$x'");
        assert_eq!(quote(TaskShellDialect::PowerShell, "a'b;$x")?, "'a''b;$x'");
        assert_eq!(quote(TaskShellDialect::Cmd, "C:\\a b")?, "\"C:\\a b\"");
        for value in ["%PATH%", "!value!", "a&b", "a\"b", "a\nb", "a^b"] {
            assert_eq!(
                quote(TaskShellDialect::Cmd, value),
                Err(TaskError::UnsafeCmdValue)
            );
        }
        assert_eq!(encoded_powershell("A"), "QQA=");
        assert_eq!(encoded_powershell("AB"), "QQBCAA==");
        assert_eq!(encoded_powershell("\u{1f600}"), "PdgA3g==");
        Ok(())
    }
}
