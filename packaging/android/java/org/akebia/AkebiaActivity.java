package org.akebia;

import android.Manifest;
import android.app.AlertDialog;
import android.app.NativeActivity;
import android.content.DialogInterface;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.provider.OpenableColumns;
import android.provider.Settings;
import android.text.InputType;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.widget.EditText;
import android.widget.FrameLayout;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * The only Java in Akebia, and it is here under protest.
 *
 * <p>Android does not let an application walk the storage any more: to read a
 * file the user has to hand it over through the system's own picker. That picker
 * answers by calling {@code onActivityResult}, and {@code NativeActivity} does
 * not forward that call to native code — which is why a subclass has to exist at
 * all.
 *
 * <p>Everything it does is: open the picker, copy whatever came back into the
 * folder the cartridges live in, and leave the path where the emulator can pick
 * it up. The emulator asks for it once per frame, which is cheaper than it
 * sounds and saves registering native methods for a callback.
 *
 * <p>Saying where that folder is turns out to be the other thing only Java can
 * do, and the reason is the saved games: the application's own folder is erased
 * with the application, so the folder is asked for out in the shared storage,
 * which is not.
 *
 * <p>And typing is the third. A native activity has no editable view in it, so
 * an input method has nowhere to put what is typed: the keyboard comes up and
 * the characters go nowhere. Anything the emulator has to be told in words — so
 * far, the address of the machine at the other end of the link cable — is
 * therefore asked for in a dialog from here. See {@link #askForAddress}.
 */
public class AkebiaActivity extends NativeActivity {

    private static final int PICK_ROM = 1;

    /** Asking for the storage, on a telephone older than Android 11. */
    private static final int ASK_STORAGE = 2;

    /**
     * The folder cartridges and saved games live in, at the top of the shared
     * storage — {@code /sdcard/Akebia} on nearly every telephone.
     *
     * <p>It is out there and not in the application's own folder for one reason:
     * Android erases everything the application owns when it is uninstalled, and
     * what it erases includes the saved game. A folder in the shared storage
     * belongs to whoever holds the telephone, survives the uninstall, and can be
     * copied off over USB without asking Akebia for anything.
     */
    private static final String SHARED = "Akebia";

    /** Path of the ROM just imported, waiting to be collected. */
    private String imported;

    /**
     * What the address box was closed with, waiting to be collected.
     *
     * <p>Empty when it was closed without an address, which is not the same as
     * null: a box that was cancelled has been answered, and the emulator has to
     * stop waiting on it.
     */
    private String address;

    /**
     * Whether what was inside the application's folder has already been carried
     * out to the shared one.
     *
     * <p>Read and written from the emulator's thread only, which is the one that
     * asks where the cartridges are.
     */
    private boolean migrated;

    /**
     * How deep the system's own furniture reaches into the window, in pixels,
     * as {left, top, right, bottom}.
     *
     * <p>A native activity is handed the whole display — clock, battery and
     * navigation bar included — so anything drawn at the very top or the very
     * bottom comes out underneath them. Asking is the only way to know how much
     * to keep clear, and the answer changes when the telephone is turned.
     */
    private final int[] insets = new int[4];

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        View decor = getWindow().getDecorView();
        decor.setOnApplyWindowInsetsListener(new View.OnApplyWindowInsetsListener() {
            @Override
            public WindowInsets onApplyWindowInsets(View view, WindowInsets applied) {
                remember(applied);
                // Passed on untouched: consuming them would leave the rest of
                // the window believing there is nothing in its way.
                return applied;
            }
        });
        decor.requestApplyInsets();
    }

    @Override
    public void onWindowFocusChanged(boolean focused) {
        super.onWindowFocusChanged(focused);
        // Coming back from the file picker, or from a rotation, the numbers may
        // well be different ones.
        if (focused) {
            getWindow().getDecorView().requestApplyInsets();
        }
    }

    /** The insets, in pixels: {left, top, right, bottom}. */
    public int[] getInsets() {
        synchronized (insets) {
            return new int[] {insets[0], insets[1], insets[2], insets[3]};
        }
    }

    // The pre-Android-11 way of asking is deprecated, and it is used anyway:
    // there is no other one on a telephone that old, and it is guarded.
    @SuppressWarnings("deprecation")
    private void remember(WindowInsets applied) {
        int[] measured =
                Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
                        ? bars(applied)
                        : new int[] {
                            applied.getSystemWindowInsetLeft(),
                            applied.getSystemWindowInsetTop(),
                            applied.getSystemWindowInsetRight(),
                            applied.getSystemWindowInsetBottom()
                        };
        synchronized (insets) {
            System.arraycopy(measured, 0, insets, 0, 4);
        }
    }

    /**
     * The same question in the way Android 11 and later want it asked.
     *
     * <p>It is a method of its own so that a telephone older than that never
     * looks inside it: the classes named here do not exist there, and code that
     * is never entered is never verified either.
     */
    private static int[] bars(WindowInsets applied) {
        Insets covered =
                applied.getInsets(
                        WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
        return new int[] {covered.left, covered.top, covered.right, covered.bottom};
    }

    /** Opens the system picker. Called from the emulator's thread. */
    public void openRomPicker() {
        // `startActivityForResult` belongs to the UI thread, and the emulator
        // runs on another one: `android_main` is given a thread of its own.
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                // Cartridges have no MIME type of their own and the system does
                // not guess one from the extension, so narrowing this would hide
                // exactly the files we are after.
                intent.setType("*/*");
                startActivityForResult(intent, PICK_ROM);
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != PICK_ROM || result != RESULT_OK || data == null) {
            return;
        }
        Uri uri = data.getData();
        if (uri == null) {
            return;
        }
        String path = copyIn(uri);
        synchronized (this) {
            imported = path;
        }
    }

    /**
     * The ROM imported since this was last asked, or null.
     *
     * <p>It hands it over once: reading it clears it, so a ROM does not get
     * loaded again on every frame that follows.
     */
    public synchronized String pollImportedRom() {
        String path = imported;
        imported = null;
        return path;
    }

    /**
     * Asks for the address of the machine to link to, with a real keyboard.
     *
     * <p>It is here for the same kind of reason the picker is: a {@code
     * NativeActivity} has no editable view in it, so an input method has
     * nothing to write into. Android will raise the keyboard over one happily
     * enough — {@code showSoftInput} works — but there is no {@code
     * InputConnection} behind it, and what the emulator gets back is nothing at
     * all. From the outside that is a text box that cannot be typed in, and a
     * Connect button that does nothing.
     *
     * <p>A dialog belongs to Java, has a real {@link EditText} in it, and comes
     * with the clipboard as well as the keyboard — which matters, because an
     * address is the kind of thing that arrives in a message.
     *
     * <p>The answer does not come back from here; it arrives later, through
     * {@link #pollAddress}, exactly like the picker's.
     *
     * @param current what to start it off with, so a second attempt at the same
     *     address is not typed twice. May be null.
     */
    public void askForAddress(final String current) {
        // Whatever a previous box was closed with, before a new one can be
        // closed with anything. Without this, a box opened and then abandoned —
        // leaving the game for the list is one way— would have its answer
        // handed back as the answer to the next one, and the emulator would
        // dial an address nobody typed.
        forget();
        // Dialogs belong to the UI thread, and the emulator runs on another
        // one: `android_main` is given a thread of its own.
        runOnUiThread(
                new Runnable() {
                    @Override
                    public void run() {
                        show(current);
                    }
                });
    }

    /**
     * The address the box was closed with, or null while it is still open.
     *
     * <p>Handed over once, like the imported ROM: reading it clears it.
     */
    public synchronized String pollAddress() {
        String typed = address;
        address = null;
        return typed;
    }

    /** Records what the box was closed with. Null and blank both mean nothing. */
    private synchronized void answered(String typed) {
        address = typed == null ? "" : typed.trim();
    }

    /** Back to having no answer at all, which is not the same as an empty one. */
    private synchronized void forget() {
        address = null;
    }

    /** The box itself, on the thread that is allowed to put one up. */
    private void show(String current) {
        final EditText box = new EditText(this);
        // A URI and not a number: an address may be a name, and the numeric
        // keyboard has neither letters nor the colon that a port needs. What
        // the variation is really for is the rest of it — no autocorrect, no
        // capital at the start, and a dot on the main row.
        box.setInputType(
                InputType.TYPE_CLASS_TEXT
                        | InputType.TYPE_TEXT_VARIATION_URI
                        | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        box.setSingleLine(true);
        box.setHint("192.168.1.20");
        box.setText(current == null ? "" : current);
        box.setSelection(box.getText().length());

        // A bare EditText sits flush against the sides of a dialog. The padding
        // is in points and the method wants pixels, which is what the density
        // is for.
        int margin = Math.round(24 * getResources().getDisplayMetrics().density);
        FrameLayout frame = new FrameLayout(this);
        frame.setPadding(margin, margin / 2, margin, 0);
        frame.addView(box);

        // Whether the box was closed by the Connect button or by anything else.
        // The dismiss listener runs either way and would otherwise wipe out the
        // address the button had just recorded.
        final boolean[] taken = {false};
        AlertDialog dialog =
                new AlertDialog.Builder(this)
                        .setTitle("Connect to")
                        .setView(frame)
                        .setPositiveButton(
                                "Connect",
                                new DialogInterface.OnClickListener() {
                                    @Override
                                    public void onClick(DialogInterface which, int button) {
                                        taken[0] = true;
                                        answered(box.getText().toString());
                                    }
                                })
                        .setNegativeButton("Cancel", null)
                        .setOnDismissListener(
                                new DialogInterface.OnDismissListener() {
                                    @Override
                                    public void onDismiss(DialogInterface which) {
                                        // Cancelled, dismissed with the back
                                        // key, or tapped away. All three are an
                                        // answer of «no address», and every one
                                        // of them has to be one: an emulator
                                        // still waiting on a box that is no
                                        // longer there would wait for ever.
                                        if (!taken[0]) {
                                            answered(null);
                                        }
                                    }
                                })
                        .create();
        // Asked for outright rather than left to the input method's judgement:
        // the box is the only thing in the dialog, so there is nothing else the
        // person opening it could have meant to do.
        dialog.getWindow()
                .setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE);
        dialog.show();
    }

    /** The folder the imported cartridges live in. */
    public String getRomsDir() {
        return romsDir().getAbsolutePath();
    }

    /**
     * Whether the saved games are being kept where the uninstall cannot reach.
     *
     * <p>The permission is the usual reason for a no, but it is deliberately not
     * what is asked here: what the emulator shows on the strength of this answer
     * is a warning about losing saved games, and a folder that could not be made
     * loses them just as thoroughly as one that was never allowed.
     */
    public boolean hasStorage() {
        return sharedDir() != null;
    }

    /**
     * Whether the shared storage may be written to.
     *
     * <p>Two different questions under one name. From Android 11 it is the "all
     * files access" the user grants in the system's settings; before that it is
     * the old runtime permission, which a dialog is enough for.
     */
    private boolean granted() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            return manager();
        }
        return checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE)
                == PackageManager.PERMISSION_GRANTED;
    }

    /**
     * The Android 11 form of the question, kept apart so that a telephone older
     * than that never looks inside it.
     */
    private static boolean manager() {
        return Environment.isExternalStorageManager();
    }

    /** Asks for it. Called from the emulator's thread. */
    public void requestStorage() {
        runOnUiThread(
                new Runnable() {
                    @Override
                    public void run() {
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                            askManager();
                        } else {
                            requestPermissions(
                                    new String[] {Manifest.permission.WRITE_EXTERNAL_STORAGE},
                                    ASK_STORAGE);
                        }
                    }
                });
    }

    /**
     * Sends the user to the settings screen where "all files access" is given.
     *
     * <p>There is no dialog for this one: from Android 11 the system will not
     * let an application ask for it in passing, and the only way through is the
     * settings. Nothing is waited for here — the answer is noticed later, when
     * the emulator asks {@link #hasStorage} again on coming back.
     */
    private void askManager() {
        Uri self = Uri.fromParts("package", getPackageName(), null);
        try {
            startActivity(
                    new Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, self));
        } catch (Exception e) {
            // A few telephones carry no screen for one application on its own.
            // The list of every application is a worse place to be left in, and
            // it is what there is.
            try {
                startActivity(new Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION));
            } catch (Exception nowhere) {
                // Nothing more to try: Akebia goes on saving in its own folder,
                // which works and is only lost on uninstalling.
            }
        }
    }

    /**
     * Copies the document into the cartridge folder and returns where it landed,
     * or null if it could not be read.
     *
     * <p>It is copied and not merely remembered because the permission over the
     * URI dies with the process: what the user picked today would not open
     * tomorrow. A copy is a file like any other, and the rest of Akebia —the
     * saved game beside it, above all— goes on knowing nothing about Android.
     */
    private String copyIn(Uri uri) {
        File out = new File(romsDir(), fileName(uri));
        try (InputStream in = getContentResolver().openInputStream(uri);
                OutputStream os = new FileOutputStream(out)) {
            if (in == null) {
                return null;
            }
            byte[] buffer = new byte[64 * 1024];
            int read;
            while ((read = in.read(buffer)) > 0) {
                os.write(buffer, 0, read);
            }
        } catch (Exception e) {
            return null;
        }
        return out.getAbsolutePath();
    }

    /**
     * The name the picker shows for the document.
     *
     * <p>Anything that looks like a path is thrown away: the name arrives from
     * whichever application published the document and it is not to be trusted
     * to stay inside the folder it is joined to.
     */
    private String fileName(Uri uri) {
        String name = null;
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    name = cursor.getString(column);
                }
            }
        } catch (Exception e) {
            name = null;
        }
        if (name != null) {
            name = new File(name).getName();
        }
        if (name == null || name.isEmpty() || name.equals(".") || name.equals("..")) {
            name = "cartridge.gb";
        }
        return name;
    }

    /**
     * Where the cartridges and their saved games are kept.
     *
     * <p>The shared folder if there is one to be had, and the application's own
     * otherwise. The second is not a lesser arrangement in the day to day —it is
     * where everything used to live and everything worked— but it goes with the
     * application when it is uninstalled, saved games and all.
     */
    private File romsDir() {
        File shared = sharedDir();
        if (shared != null) {
            migrate(shared);
            return shared;
        }
        File dir = new File(getFilesDir(), "roms");
        dir.mkdirs();
        return dir;
    }

    /**
     * The folder in the shared storage, or null if it cannot be used.
     *
     * <p>Null covers three cases that come to the same thing here: the access
     * has not been granted, the storage is not mounted, and the folder could not
     * be made.
     */
    // `getExternalStorageDirectory` is deprecated in favour of asking through
    // `MediaStore`, and that is not an option: what is wanted is a real path,
    // because the emulator underneath opens files and knows nothing of Android.
    @SuppressWarnings("deprecation")
    private File sharedDir() {
        if (!granted()
                || !Environment.MEDIA_MOUNTED.equals(Environment.getExternalStorageState())) {
            return null;
        }
        File dir = new File(Environment.getExternalStorageDirectory(), SHARED);
        return dir.isDirectory() || dir.mkdirs() ? dir : null;
    }

    /**
     * Carries what is in the application's folder out to the shared one, once.
     *
     * <p>This is what the access being granted actually does for somebody who
     * was already playing: the cartridges they had imported and the games they
     * had saved move out to where the next uninstall cannot take them. It is a
     * copy and a delete and not a rename, because the two are on different
     * filesystems and no rename crosses that.
     *
     * <p>Nothing already in the shared folder is overwritten. A file out there
     * with the same name is, as far as this can tell, the saved game of a
     * previous installation — which is the very thing this whole arrangement
     * exists to protect. The copy inside is left alone rather than deleted, so
     * that whichever of the two is worth keeping is still there to be looked at.
     *
     * <p>It runs from the list of cartridges and never mid-game: it is reached
     * through {@link #getRomsDir}, which nothing asks for while a console is
     * running. Moving a `.sav` out from under a game in progress would leave the
     * autosave writing to a file nobody would read again.
     */
    private void migrate(File shared) {
        if (migrated) {
            return;
        }
        migrated = true;
        File[] inside = new File(getFilesDir(), "roms").listFiles();
        if (inside == null) {
            return;
        }
        for (File file : inside) {
            File out = new File(shared, file.getName());
            if (!file.isFile() || out.exists()) {
                continue;
            }
            if (copy(file, out)) {
                file.delete();
            }
        }
    }

    /** Copies one file over another that does not exist yet. */
    private boolean copy(File from, File to) {
        try (InputStream in = new FileInputStream(from);
                OutputStream os = new FileOutputStream(to)) {
            byte[] buffer = new byte[64 * 1024];
            int read;
            while ((read = in.read(buffer)) > 0) {
                os.write(buffer, 0, read);
            }
        } catch (Exception e) {
            // Half a file is worse than none: what is left behind would be read
            // as a saved game later on.
            to.delete();
            return false;
        }
        return true;
    }
}
