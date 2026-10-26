//! The little that has to be asked of Java.
//!
//! Every call to [`AkebiaActivity`] is here because Android leaves no other
//! way: a native application cannot open the system's file picker on its own,
//! cannot hear its answer, cannot ask where its own folder is without going
//! through the framework — and cannot be typed into at all, because a native
//! activity has no editable view for an input method to write in.
//!
//! [`AkebiaActivity`]: ../../../packaging/android/java/org/akebia/AkebiaActivity.java

use std::path::PathBuf;

use jni::objects::{JIntArray, JObject, JString};
use jni::strings::JNIStr;
use jni::{jni_sig, jni_str, Env, JavaVM};
use winit::platform::android::activity::AndroidApp;

/// A way through to the activity.
pub struct Java {
    app: AndroidApp,
    vm: JavaVM,
}

impl Java {
    pub fn new(app: &AndroidApp) -> Self {
        // SAFETY: the pointer is the one the system handed `android-activity`,
        // and the virtual machine outlives the activity, which outlives this.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        Self { app: app.clone(), vm }
    }

    /// Asks for the system picker to be opened.
    ///
    /// The answer does not come back from here: the user may spend a minute
    /// looking around, and meanwhile the emulator has frames to draw. It arrives
    /// later, through [`Java::imported_rom`].
    pub fn open_picker(&self) -> Result<(), String> {
        self.attach(|env, activity| {
            env.call_method(activity, jni_str!("openRomPicker"), jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// The cartridge imported since this was last asked, if any.
    ///
    /// Asked once per frame. It costs a call into an already attached thread and
    /// a null check, which is less than the copy it saves us from arranging any
    /// other way.
    pub fn imported_rom(&self) -> Option<PathBuf> {
        self.path(jni_str!("pollImportedRom"))
    }

    /// The folder the imported cartridges live in.
    pub fn roms_dir(&self) -> Option<PathBuf> {
        self.path(jni_str!("getRomsDir"))
    }

    /// Puts up a box to type the address of the other machine into.
    ///
    /// It is Java's and not the interface's for a reason that is not taste:
    /// `egui` has a text field and on a telephone nothing can be typed into it.
    /// The window is a `NativeActivity`, which has no editable view, so
    /// `android-activity` has nothing to give an input method — its
    /// `text_input_state` is a stub that answers with an empty string, and
    /// `set_ime_editor_info` does nothing at all. The keyboard comes up and the
    /// characters go nowhere.
    ///
    /// Like the file picker, the answer does not come back from here: it is
    /// collected later, by [`Java::typed_address`].
    pub fn ask_for_address(&self, current: &str) -> Result<(), String> {
        self.attach(|env, activity| {
            let current = env.new_string(current)?;
            env.call_method(
                activity,
                jni_str!("askForAddress"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[(&current).into()],
            )?;
            Ok(())
        })
    }

    /// What the address box was closed with, once it has been closed.
    ///
    /// `None` while it is still open, or was never opened. An empty string when
    /// it was closed without an address, which is a different thing and has to
    /// stay different: it is how a cancelled box says so, and something has to
    /// stop waiting on it.
    pub fn typed_address(&self) -> Option<String> {
        self.text(jni_str!("pollAddress"))
    }

    /// Whether the cartridges can be kept out of the application's own folder.
    ///
    /// It is the difference between a saved game that survives uninstalling
    /// Akebia and one that does not, and nothing else: with it or without it the
    /// emulator plays and saves the same way.
    pub fn storage_granted(&self) -> bool {
        self.flag(jni_str!("hasStorage"))
    }

    /// Asks for it, which means sending the user to the system's settings.
    ///
    /// Like the file picker, the answer does not come back from here: it is seen
    /// on the way back, the next time [`Java::storage_granted`] is asked.
    pub fn request_storage(&self) -> Result<(), String> {
        self.attach(|env, activity| {
            env.call_method(activity, jni_str!("requestStorage"), jni_sig!("()V"), &[])?;
            Ok(())
        })
    }

    /// How far the clock, the navigation bar and the camera's hole reach into
    /// the window: left, top, right and bottom, in pixels.
    ///
    /// All zeroes if it could not be asked, which is the honest answer for a
    /// window nothing is covering and the harmless one otherwise: at worst the
    /// picture goes back to where it was before any of this.
    pub fn insets(&self) -> [f32; 4] {
        let measured = self.attach(|env, activity| {
            let value = env.call_method(activity, jni_str!("getInsets"), jni_sig!("()[I"), &[])?;
            let object = value.l()?;
            let array = env.as_cast::<JIntArray>(&object)?;
            let mut edges = [0i32; 4];
            array.get_region(env, 0, &mut edges)?;
            Ok(edges)
        });
        match measured {
            Ok(edges) => edges.map(|edge| edge as f32),
            Err(message) => {
                log::error!("getInsets: {message}");
                [0.0; 4]
            }
        }
    }

    /// Calls a method with no arguments that answers yes or no.
    ///
    /// A question that could not be asked is a no. Every one of them is about
    /// something Akebia may do and not about something it needs, so the false
    /// leaves the emulator on the path that asks for nothing.
    fn flag(&self, method: &'static JNIStr) -> bool {
        let answer = self.attach(|env, activity| {
            env.call_method(activity, method, jni_sig!("()Z"), &[])?.z()
        });
        match answer {
            Ok(flag) => flag,
            Err(message) => {
                log::error!("{method}: {message}");
                false
            }
        }
    }

    /// Calls a method with no arguments that answers with a path, or with null.
    fn path(&self, method: &'static JNIStr) -> Option<PathBuf> {
        self.text(method).map(PathBuf::from)
    }

    /// Calls a method with no arguments that answers with a string, or with
    /// null.
    ///
    /// An empty string is not a null and does not become one: the address box
    /// uses the difference to say it was closed without an answer.
    fn text(&self, method: &'static JNIStr) -> Option<String> {
        let found = self.attach(|env, activity| {
            let value = env.call_method(activity, method, jni_sig!("()Ljava/lang/String;"), &[])?;
            let object = value.l()?;
            if object.is_null() {
                return Ok(None);
            }
            Ok(Some(env.as_cast::<JString>(&object)?.to_string()))
        });
        match found {
            Ok(text) => text,
            Err(message) => {
                log::error!("{method}: {message}");
                None
            }
        }
    }

    /// Runs something with the activity in hand.
    ///
    /// The thread is already attached —`android-activity` attaches the one it
    /// runs `android_main` on— so this costs a lookup in thread-local storage
    /// and no more.
    fn attach<T>(
        &self,
        body: impl FnOnce(&mut Env, &JObject) -> Result<T, jni::errors::Error>,
    ) -> Result<T, String> {
        self.vm
            .attach_current_thread(|env| {
                // SAFETY: the activity belongs to `android-activity`, which
                // holds it for as long as there is an activity at all; it is
                // only borrowed here, never released.
                let activity = unsafe { JObject::from_raw(env, self.app.activity_as_ptr().cast()) };
                body(env, &activity)
            })
            .map_err(|e: jni::errors::Error| e.to_string())
    }
}
