//! Saving a setting: the palette's theme and refresh commands write through
//! here to `settings.toml`, unless `config.toml` sets the key, in which case
//! the change holds for this session only.

use crate::Diffui;
use crate::config::Setting;
use crate::prose::Prose;

impl Diffui {
    /// Save `setting`, already applied, for the next launch.
    pub(crate) fn persist_setting(&mut self, setting: Setting) {
        let key = setting.key();
        if self.config.locked.contains(key) {
            self.push_error_toast(
                Prose::from(format!("{} is set in ", key.label())).path("config.toml"),
                "Changed for this session only",
            );
            return;
        }
        if let Err(error) = self.config_files.write(&setting) {
            self.push_error_toast("Couldn't save settings", &error);
        }
    }
}
