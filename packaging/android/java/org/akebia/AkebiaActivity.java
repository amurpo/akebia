package org.akebia;

import android.app.NativeActivity;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.OpenableColumns;
import android.view.View;
import android.view.WindowInsets;

import java.io.File;
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
 * application's own folder, and leave the path where the emulator can pick it
 * up. The emulator asks for it once per frame, which is cheaper than it sounds
 * and saves registering native methods for a callback.
 */
public class AkebiaActivity extends NativeActivity {

    private static final int PICK_ROM = 1;

    /** Path of the ROM just imported, waiting to be collected. */
    private String imported;

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

    /** The folder the imported cartridges live in. */
    public String getRomsDir() {
        return romsDir().getAbsolutePath();
    }

    /**
     * Copies the document into the application's folder and returns where it
     * landed, or null if it could not be read.
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

    private File romsDir() {
        File dir = new File(getFilesDir(), "roms");
        dir.mkdirs();
        return dir;
    }
}
