#[cfg(windows)]
pub(crate) const BACKGROUND_ARG: &str = "--background";
#[cfg(windows)]
pub(crate) const REMOVE_STARTUP_ARG: &str = "--remove-own-startup";

#[cfg(windows)]
pub(crate) fn remove_own_registration() -> anyhow::Result<()> {
    platform::remove_matching_registration()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Registration {
    Disabled,
    Current,
    DifferentCommand,
}

#[cfg(windows)]
mod platform {
    use super::{BACKGROUND_ARG, Registration};
    use anyhow::{Result, ensure};
    use std::{os::windows::ffi::OsStrExt, path::Path};
    use windows::core::{Error, HRESULT};
    use windows_registry::{CURRENT_USER, Type};
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_UNSUPPORTED_TYPE},
        System::Registry::{KEY_QUERY_VALUE, KEY_SET_VALUE},
    };

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "S3FS";
    const MAX_COMMAND_UNITS: usize = 261;

    pub(super) fn registered() -> Result<Registration> {
        registration(RUN, NAME, &std::env::current_exe()?)
    }

    pub(super) fn remove_matching_registration() -> Result<()> {
        // An installed and a portable copy share the same Run value.
        if registered()? == Registration::Current {
            set(false)?;
        }
        Ok(())
    }

    fn registration(path: &str, name: &str, executable: &Path) -> Result<Registration> {
        let value = match CURRENT_USER
            .options()
            .access(KEY_QUERY_VALUE)
            .open(path)
            .and_then(|key| key.get_value(name))
        {
            Ok(value) => value,
            Err(error) if has_code(&error, ERROR_FILE_NOT_FOUND) => {
                return Ok(Registration::Disabled);
            }
            Err(error) if has_code(&error, ERROR_MORE_DATA) => {
                return Ok(Registration::DifferentCommand);
            }
            Err(error) => return Err(error.into()),
        };
        if value.ty() != Type::String {
            return Err(std::io::Error::from_raw_os_error(ERROR_UNSUPPORTED_TYPE as i32).into());
        }
        // RegGetValue supplied one trailing NUL if the stored string omitted it.
        let command = value.as_wide();
        let text = command.strip_suffix(&[0]).unwrap_or(command);
        if text.len() >= MAX_COMMAND_UNITS {
            return Ok(Registration::DifferentCommand);
        }
        if text.iter().copied().chain([0]).eq(command_for(executable)) {
            return Ok(Registration::Current);
        }
        if text.iter().copied().eq(quoted_path(executable)) {
            set_value(path, name, Some(executable))?;
            return Ok(Registration::Current);
        }
        Ok(Registration::DifferentCommand)
    }

    fn has_code(error: &Error, code: u32) -> bool {
        error.code() == HRESULT::from_win32(code)
    }

    fn command_for(executable: &Path) -> Vec<u16> {
        quoted_path(executable)
            .chain(" ".encode_utf16())
            .chain(BACKGROUND_ARG.encode_utf16())
            .chain([0])
            .collect()
    }

    fn quoted_path(executable: &Path) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(u16::from(b'"'))
            .chain(executable.as_os_str().encode_wide())
            .chain([u16::from(b'"')])
    }

    pub(super) fn set(enabled: bool) -> Result<()> {
        let exe = enabled.then(std::env::current_exe).transpose()?;
        set_value(RUN, NAME, exe.as_deref())
    }

    fn set_value(path: &str, name: &str, executable: Option<&Path>) -> Result<()> {
        if let Some(exe) = executable {
            return write_command(path, name, &command_for(exe));
        }
        match CURRENT_USER
            .options()
            .access(KEY_SET_VALUE)
            .open(path)
            .and_then(|key| key.remove_value(name))
        {
            Ok(()) => Ok(()),
            Err(error) if has_code(&error, ERROR_FILE_NOT_FOUND) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_command(path: &str, name: &str, command: &[u16]) -> Result<()> {
        ensure!(
            command.len() <= MAX_COMMAND_UNITS,
            "程序路径过长，无法注册登录启动。"
        );
        let bytes: Vec<_> = command.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        CURRENT_USER
            .options()
            .access(KEY_SET_VALUE)
            .create()
            .open(path)?
            .set_bytes(name, Type::String, &bytes)?;
        Ok(())
    }

    #[cfg(test)]
    #[test]
    fn registry_registration_round_trip() -> Result<()> {
        let path = format!(r"Software\S3FS-Test-{}", uuid::Uuid::new_v4());
        let result = (|| {
            let executable = Path::new(r"C:\Program Files\测试目录\s3fs.exe");
            let previous = Path::new(r"C:\Old installation\s3fs.exe");
            ensure!(registration(&path, NAME, executable)? == Registration::Disabled);
            set_value(&path, NAME, Some(previous))?;
            ensure!(registration(&path, NAME, executable)? == Registration::DifferentCommand);
            let legacy = quoted_path(executable).chain([0]).collect::<Vec<_>>();
            write_command(&path, NAME, &legacy)?;
            ensure!(registration(&path, NAME, executable)? == Registration::Current);
            ensure!(
                command_for(executable)
                    == wide(r#""C:\Program Files\测试目录\s3fs.exe" --background"#)
            );
            let mut max_length_command = [u16::from(b'x'); MAX_COMMAND_UNITS];
            max_length_command[MAX_COMMAND_UNITS - 1] = 0;
            write_command(&path, NAME, &max_length_command)?;
            ensure!(registration(&path, NAME, executable)? == Registration::DifferentCommand);
            ensure!(write_command(&path, NAME, &[0; MAX_COMMAND_UNITS + 1]).is_err());
            set_value(&path, NAME, None)?;
            set_value(&path, NAME, None)?;
            ensure!(registration(&path, NAME, executable)? == Registration::Disabled);
            Ok(())
        })();
        let _ = CURRENT_USER.remove_tree(&path);
        result
    }

    #[cfg(test)]
    #[test]
    #[ignore = "temporarily writes an isolated value in the Windows Run key"]
    fn windows_run_registration() -> Result<()> {
        let name = format!("S3FS-Test-{}", uuid::Uuid::new_v4());
        let executable = std::env::current_exe()?;
        ensure!(registration(RUN, &name, &executable)? == Registration::Disabled);
        set_value(RUN, &name, Some(&executable))?;
        let observed = registration(RUN, &name, &executable);
        set_value(RUN, &name, None)?;
        ensure!(observed? == Registration::Current);
        ensure!(registration(RUN, &name, &executable)? == Registration::Disabled);
        Ok(())
    }

    #[cfg(test)]
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain([0]).collect()
    }
}

pub(crate) fn registered() -> anyhow::Result<Registration> {
    #[cfg(windows)]
    {
        platform::registered()
    }
    #[cfg(not(windows))]
    Ok(Registration::Disabled)
}

pub(crate) fn set(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    return platform::set(enabled);
    #[cfg(not(windows))]
    {
        let _ = enabled;
        anyhow::bail!("当前系统不支持登录启动注册。")
    }
}
