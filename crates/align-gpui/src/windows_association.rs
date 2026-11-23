//! Per-user file association installed and removed by the Windows package.

use std::{ffi::OsString, io, ptr};

use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use winreg::{
    RegKey, RegValue,
    enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_NONE},
};

const CLASSES: &str = r"Software\Classes";
const EXTENSION: &str = ".align";
const PROG_ID: &str = "Align.Project.1";
const OPEN_WITH: &str = r".align\OpenWithProgids";

pub fn run_installer_hooks() {
    velopack::VelopackApp::build()
        .on_after_install_fast_callback(|_| report(register()))
        .on_after_update_fast_callback(|_| report(register()))
        .on_before_uninstall_fast_callback(|_| report(unregister()))
        .run();
}

fn report(result: io::Result<()>) {
    // Refresh Explorer's cache after the installer changes the association.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            ptr::null(),
            ptr::null(),
        )
    };
    if let Err(error) = result {
        eprintln!("Could not update the Align project file association: {error}");
    }
}

fn register() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = OsString::from("\"");
    command.push(&executable);
    command.push("\" \"%1\"");

    let (classes, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(CLASSES)?;
    let (project, _) = classes.create_subkey(PROG_ID)?;
    project.set_value("", &"Align Project")?;
    project
        .create_subkey(r"shell\open\command")?
        .0
        .set_value("", &command)?;
    classes.create_subkey(OPEN_WITH)?.0.set_raw_value(
        PROG_ID,
        &RegValue {
            bytes: Vec::new(),
            vtype: REG_NONE,
        },
    )?;

    let current = RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey(EXTENSION)
        .and_then(|key| key.get_raw_value(""));
    if current.is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        classes
            .create_subkey(EXTENSION)?
            .0
            .set_value("", &PROG_ID)?;
    }
    Ok(())
}

fn unregister() -> io::Result<()> {
    let classes = RegKey::predef(HKEY_CURRENT_USER);
    ignore_missing(classes.delete_subkey_all(format!(r"{CLASSES}\{PROG_ID}")))?;
    // Explorer ignores an extension's default ProgID after its registration
    // is removed. Leave the default alone so uninstall preserves user choices.
    let open_with = format!(r"{CLASSES}\{OPEN_WITH}");
    ignore_missing(
        classes
            .open_subkey_with_flags(open_with, KEY_SET_VALUE)
            .and_then(|key| key.delete_value(PROG_ID)),
    )
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
