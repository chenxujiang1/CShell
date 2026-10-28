//! Built-in templates create drafts, never run or grant retry permission.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinTaskTemplate {
    PosixFileExists,
    PosixDirectoryExists,
    PosixServiceActive,
    PosixDiskUsage,
    PowerShellFileExists,
    PowerShellServiceRunning,
}
impl BuiltinTaskTemplate {
    pub const ALL: [Self; 6] = [
        Self::PosixFileExists,
        Self::PosixDirectoryExists,
        Self::PosixServiceActive,
        Self::PosixDiskUsage,
        Self::PowerShellFileExists,
        Self::PowerShellServiceRunning,
    ];
    pub fn slug(self) -> &'static str {
        match self {
            Self::PosixFileExists => "posix-file-exists",
            Self::PosixDirectoryExists => "posix-directory-exists",
            Self::PosixServiceActive => "posix-service-active",
            Self::PosixDiskUsage => "posix-disk-usage",
            Self::PowerShellFileExists => "powershell-file-exists",
            Self::PowerShellServiceRunning => "powershell-service-running",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::PosixFileExists => "POSIX · 文件存在检查",
            Self::PosixDirectoryExists => "POSIX · 目录存在检查",
            Self::PosixServiceActive => "POSIX · systemd 服务运行检查",
            Self::PosixDiskUsage => "POSIX · 磁盘占用报告",
            Self::PowerShellFileExists => "PowerShell · 文件存在检查",
            Self::PowerShellServiceRunning => "PowerShell · 服务运行检查",
        }
    }
    pub fn parse(slug: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|template| template.slug() == slug)
    }
    pub fn definition(self) -> TaskDefinition {
        let mut variables = BTreeMap::new();
        let mut environment = BTreeMap::new();
        let (shell, command) = match self {
            Self::PosixFileExists | Self::PosixDirectoryExists => {
                variables.insert("path".into(), "/path/to/check".into());
                (
                    RemoteShellProfile::Posix,
                    TaskCommand::Invocation {
                        program: "test".into(),
                        arguments: vec![
                            TaskArgument::Literal {
                                text: if self == Self::PosixFileExists {
                                    "-f"
                                } else {
                                    "-d"
                                }
                                .into(),
                            },
                            TaskArgument::Variable {
                                name: "path".into(),
                            },
                        ],
                    },
                )
            }
            Self::PosixServiceActive => {
                variables.insert("service".into(), "example.service".into());
                (
                    RemoteShellProfile::Posix,
                    TaskCommand::Invocation {
                        program: "systemctl".into(),
                        arguments: vec![
                            TaskArgument::Literal {
                                text: "is-active".into(),
                            },
                            TaskArgument::Literal {
                                text: "--quiet".into(),
                            },
                            TaskArgument::Literal { text: "--".into() },
                            TaskArgument::Variable {
                                name: "service".into(),
                            },
                        ],
                    },
                )
            }
            Self::PosixDiskUsage => (
                RemoteShellProfile::Posix,
                TaskCommand::Invocation {
                    program: "df".into(),
                    arguments: vec![TaskArgument::Literal { text: "-h".into() }],
                },
            ),
            Self::PowerShellFileExists => {
                environment.insert("CSHELL_CHECK_PATH".into(), r"C:\path\to\check".into());
                (RemoteShellProfile::PowerShell, TaskCommand::Script {
                    text: "if (-not (Test-Path -LiteralPath $env:CSHELL_CHECK_PATH -PathType Leaf)) { exit 1 }\nexit 0".into(),
                })
            }
            Self::PowerShellServiceRunning => {
                environment.insert("CSHELL_CHECK_SERVICE".into(), "ExampleService".into());
                (RemoteShellProfile::PowerShell, TaskCommand::Script {
                    // Compare service objects rather than passing user values as patterns.
                    text: "$service = Get-Service | Where-Object { $_.Name -eq $env:CSHELL_CHECK_SERVICE }\nif (@($service).Count -ne 1 -or $service.Status -ne 'Running') { exit 1 }\nexit 0".into(),
                })
            }
        };
        TaskDefinition {
            id: TaskId::new(),
            name: self.label().into(),
            shell,
            steps: vec![TaskStep {
                name: self.label().into(),
                command,
                success_codes: BTreeSet::from([0]),
                idempotent: true,
            }],
            variables,
            environment,
            policy: TaskPolicy::default(),
        }
    }
}
