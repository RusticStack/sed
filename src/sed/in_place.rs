// Support for in-place editing
//
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Diomidis Spinellis
//
// This file is part of the uutils sed package.
// It is licensed under the MIT License.
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use std::fs;
use std::io::stdout;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::NamedTempFile;
use uucore::display::Quotable;
use uucore::error::{FromIo, UIoError, UResult, USimpleError};

use crate::sed::command::ProcessingContext;
use crate::sed::fast_io::OutputBuffer;

/// Context for in-place editing
pub struct InPlace {
    pub output: OutputBuffer,
    pub in_place: bool,
    pub in_place_suffix: Option<String>,
    pub follow_symlinks: bool,
    pub temp_file: Option<NamedTempFile>,
    pub original_path: Option<PathBuf>,
    original_permissions: Option<fs::Permissions>,
}

impl Drop for InPlace {
    fn drop(&mut self) {
        if self.temp_file.is_some() {
            // Close the reopened output handle before dropping NamedTempFile
            // so the unpersisted temp file can always be deleted on Windows.
            self.output = OutputBuffer::new(Box::new(stdout()));
            drop(self.temp_file.take());
        }
    }
}

impl InPlace {
    /// Create an in-place editing engine based on ProcessingContext.
    /// Depending on its settings it may or may not perform in-place
    /// editing, backup the original file, or follow symlinks.
    pub fn new(context: ProcessingContext) -> Self {
        Self {
            output: OutputBuffer::new(Box::new(stdout())),
            in_place: context.in_place,
            in_place_suffix: context.in_place_suffix,
            follow_symlinks: context.follow_symlinks,
            temp_file: None,
            original_path: None,
            original_permissions: None,
        }
    }

    /// Return an OutputBuffer for outputting the edits to the specified file.
    /// The file may be a symbolic link, which will be processed according
    /// to the context specification.
    pub fn begin(&mut self, file_name: &Path) -> UResult<&mut OutputBuffer> {
        let resolved = if self.follow_symlinks {
            fs::canonicalize(file_name)
                .map_err_context(|| format!("resolving symlink {}", file_name.quote()))?
        } else {
            file_name.to_path_buf()
        };
        self.begin_resolved(&resolved)
    }

    /// Return an OutputBuffer for outputting the edits to the specified file.
    /// The passed file name should have resolved symbolic links according
    /// to the context settings.
    fn begin_resolved(&mut self, file_name: &Path) -> UResult<&mut OutputBuffer> {
        if !self.in_place {
            self.output = OutputBuffer::new(Box::new(stdout()));
            return Ok(&mut self.output);
        }

        let metadata = fs::metadata(file_name).map_err_context(|| {
            format!(
                "error Reading metadata of {} for in-place edit",
                file_name.quote()
            )
        })?;

        if !metadata.is_file() {
            return Err(USimpleError::new(
                2,
                format!(
                    "cannot in-place edit non-regular file {}",
                    file_name.quote()
                ),
            ));
        }

        let dir = file_name
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temp_file = NamedTempFile::new_in(dir)
            .map_err_context(|| format!("error creating temporary file in {}", dir.quote()))?;

        // TODO: On Unix use fchown(metadata.{uid,gid}) when uucore::libc supports it.
        #[cfg(unix)]
        let perms = fs::Permissions::from_mode(metadata.mode() & 0o7777);
        #[cfg(not(unix))]
        let perms = metadata.permissions();

        let reopened = temp_file.reopen().map_err_context(|| {
            format!(
                "error reopening temporary file {}",
                temp_file.path().quote()
            )
        })?;
        self.output = OutputBuffer::new(Box::new(reopened));
        self.temp_file = Some(temp_file);
        self.original_path = Some(file_name.to_path_buf());
        self.original_permissions = Some(perms);

        Ok(&mut self.output)
    }

    /// Finish (potentially in-place) editing.
    pub fn end(&mut self) -> UResult<()> {
        self.output.flush()?;

        if !self.in_place {
            return Ok(());
        }

        // Close our reopened write handle before renaming/persisting the temp file.
        self.output = OutputBuffer::new(Box::new(stdout()));

        let orig = self.original_path.take().expect("original_path unset");
        let temp = self.temp_file.take().expect("temp_file unset");
        let orig_perms = self.original_permissions.take();

        // Close the primary handle on the temporary file before replacing `orig`.
        let temp_path = temp.into_temp_path();

        #[cfg(unix)]
        if let Some(ref perms) = orig_perms {
            fs::set_permissions(&temp_path, perms.clone())?;
        }

        // Backup original if suffix is provided
        if let Some(ref suffix) = self.in_place_suffix {
            let mut backup_path = orig.clone();
            let file_name = backup_path
                .file_name()
                .expect("Missing file name for backup")
                .to_os_string();
            let mut backup_name = file_name;
            backup_name.push(suffix);
            backup_path.set_file_name(backup_name);

            #[cfg(windows)]
            // Try to remove to ensure the rename won't fail on Windows.
            let _ = fs::remove_file(&backup_path);

            fs::rename(&orig, &backup_path).map_err_context(|| {
                format!(
                    "error backing up {} to {}",
                    orig.quote(),
                    backup_path.quote()
                )
            })?;
        }

        persist_temp_file(
            temp_path,
            &orig,
            orig_perms.as_ref(),
            self.in_place_suffix.is_none(),
        )?;

        Ok(())
    }
}

/// Atomically persist `temp_path` onto `orig` and restore permissions where needed.
fn persist_temp_file(
    temp_path: tempfile::TempPath,
    orig: &Path,
    orig_perms: Option<&fs::Permissions>,
    target_exists: bool,
) -> UResult<()> {
    let persist_err = match temp_path.persist(orig) {
        Ok(()) => {
            #[cfg(windows)]
            if let Some(perms) = orig_perms
                && perms.readonly()
            {
                // tempfile::persist resets Windows attributes to FILE_ATTRIBUTE_NORMAL.
                fs::set_permissions(orig, perms.clone())?;
            }
            return Ok(());
        }
        Err(e) => e,
    };

    #[cfg(windows)]
    if target_exists
        && persist_err.error.kind() == std::io::ErrorKind::PermissionDenied
        && let Some(perms) = orig_perms
        && perms.readonly()
    {
        // MoveFileExW(MOVEFILE_REPLACE_EXISTING) rejects a read-only target;
        // clear the attribute without deleting `orig`, replace atomically, then restore.
        let mut writable = perms.clone();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        if fs::set_permissions(orig, writable).is_ok() {
            match persist_err.path.persist(orig) {
                Ok(()) => {
                    fs::set_permissions(orig, perms.clone())?;
                    return Ok(());
                }
                Err(retry_err) => {
                    let _ = fs::set_permissions(orig, perms.clone());
                    return Err(UIoError::new(
                        retry_err.error.kind(),
                        format!(
                            "error persisting temporary file {} to {}",
                            retry_err.path.quote(),
                            orig.quote()
                        ),
                    ));
                }
            }
        }
    }

    let _ = target_exists;
    let _ = orig_perms;
    Err(UIoError::new(
        persist_err.error.kind(),
        format!(
            "error persisting temporary file {} to {}",
            persist_err.path.quote(),
            orig.quote()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use assert_fs::fixture::PathChild;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;

    fn minimal_context() -> ProcessingContext {
        ProcessingContext {
            in_place: false,
            in_place_suffix: None,
            follow_symlinks: false,
            // fill in default values for the rest as needed
            ..Default::default()
        }
    }

    fn write_original(file: &Path, content: &str) {
        fs::write(file, content).unwrap();
    }

    fn read_file(file: &Path) -> String {
        let mut contents = String::new();
        fs::File::open(file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        contents
    }

    #[test]
    fn test_in_place_editing() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("file.txt");
        write_original(file.path(), "original\n");

        let mut ctx = minimal_context();
        ctx.in_place = true;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(file.path()).unwrap();
        writeln!(buf, "updated").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(file.path()), "updated\n");
    }

    #[test]
    fn test_in_place_backup() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("file.txt");
        let backup = temp.child("file.txt.bak");
        write_original(file.path(), "original\n");

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.in_place_suffix = Some(".bak".to_string());

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(file.path()).unwrap();
        writeln!(buf, "new content").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(file.path()), "new content\n");
        assert_eq!(read_file(backup.path()), "original\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_follow_true() {
        let temp = TempDir::new().unwrap();
        let real = temp.child("target.txt");
        let link = temp.child("link.txt");

        write_original(real.path(), "real\n");
        std::os::unix::fs::symlink(real.path(), link.path()).unwrap();

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.follow_symlinks = true;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(link.path()).unwrap();
        writeln!(buf, "changed").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(real.path()), "changed\n");
        assert!(link.path().exists()); // Symlink still exists
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_follow_false() {
        let temp = TempDir::new().unwrap();
        let real = temp.child("target.txt");
        let link = temp.child("link.txt");

        write_original(real.path(), "real\n");
        std::os::unix::fs::symlink(real.path(), link.path()).unwrap();

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.follow_symlinks = false;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(link.path()).unwrap();
        writeln!(buf, "linked").unwrap();
        inplace.end().unwrap();

        // real file should remain untouched
        assert_eq!(read_file(real.path()), "real\n");

        // link (symlink path) now contains the new content
        let contents = read_file(link.path());
        assert_eq!(contents, "linked\n");
    }

    #[test]
    fn test_no_in_place_outputs_to_stdout() {
        let mut ctx = minimal_context();
        ctx.in_place = false;

        let mut inplace = InPlace::new(ctx);
        let _buf = inplace.begin(Path::new("fake.txt")).unwrap();
        assert!(inplace.end().is_ok());
    }

    #[test]
    fn test_in_place_writes_temp_until_end_and_cleans_up_on_drop() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("file.txt");
        write_original(file.path(), "original\n");

        let mut ctx = minimal_context();
        ctx.in_place = true;

        {
            let mut inplace = InPlace::new(ctx);
            let buf = inplace.begin(file.path()).unwrap();
            writeln!(buf, "partial").unwrap();
            buf.flush().unwrap();

            // Original is untouched while editing; temp file lives in same dir.
            assert_eq!(read_file(file.path()), "original\n");
            let count = fs::read_dir(temp.path()).unwrap().count();
            assert_eq!(count, 2);
        }

        // Dropped without end(): original intact and temp file removed.
        assert_eq!(read_file(file.path()), "original\n");
        let entries: Vec<_> = fs::read_dir(temp.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_in_place_preserves_readonly_permissions() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("readonly.txt");
        write_original(file.path(), "original\n");

        let mut perms = fs::metadata(file.path()).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(file.path(), perms).unwrap();

        let mut ctx = minimal_context();
        ctx.in_place = true;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(file.path()).unwrap();
        writeln!(buf, "updated").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(file.path()), "updated\n");
        let new_perms = fs::metadata(file.path()).unwrap().permissions();
        assert!(new_perms.readonly());

        // Clear readonly so TempDir cleanup succeeds on Windows.
        #[cfg(windows)]
        {
            let mut writable = new_perms;
            #[allow(clippy::permissions_set_readonly_false)]
            writable.set_readonly(false);
            fs::set_permissions(file.path(), writable).unwrap();
        }
    }
}
