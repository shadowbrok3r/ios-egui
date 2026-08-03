package com.github.egui_mobile;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.net.VpnService;
import android.os.Build;
import android.os.ParcelFileDescriptor;
import android.util.Log;

/**
 * VpnService that establishes a tun interface and hands its file descriptor to Rust, and the
 * foreground notification that keeps the process alive. ACTION_FOREGROUND runs the notification
 * without a tun, for apps that need the process kept alive but no capture.
 */
public class EguiVpnService extends VpnService {
    private static final String TAG = "EguiVpn";

    public static final String ACTION_START = "com.github.egui_mobile.action.VPN_START";
    public static final String ACTION_FOREGROUND = "com.github.egui_mobile.action.FOREGROUND";
    /** Close the tun but stay in the foreground, for a proxy that is still running. */
    public static final String ACTION_STOP_VPN = "com.github.egui_mobile.action.VPN_STOP";
    public static final String ACTION_STOP = "com.github.egui_mobile.action.STOP";

    private static final String CHANNEL_ID = "egui_vpn";
    private static final int NOTIFICATION_ID = 7;
    private static final int SDK_UPSIDE_DOWN_CAKE = 34;

    /** The fd was detached to Rust, which owns it and closes it on stop. */
    private boolean tunOpen;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent != null ? intent.getAction() : null;
        if (ACTION_STOP.equals(action)) {
            teardown();
            stopSelf();
            return START_NOT_STICKY;
        }

        String title = extra(intent, "title", "VPN");
        String text = extra(intent, "text", "");
        if (!goForeground(title, text)) {
            stopSelf();
            return START_NOT_STICKY;
        }

        if (ACTION_STOP_VPN.equals(action)) {
            teardown();
        } else if (ACTION_START.equals(action) && !tunOpen) {
            establish(intent);
        }
        // Restarting with no intent would call into a native lib the activity may not have loaded.
        return START_NOT_STICKY;
    }

    private void establish(Intent intent) {
        int mtu = intent.getIntExtra("mtu", 1500);
        Builder builder = new Builder();
        builder.setSession(extra(intent, "session", "VPN"));
        builder.setMtu(mtu);
        builder.addAddress(extra(intent, "address", "10.7.0.1"), intent.getIntExtra("prefix", 32));
        builder.addRoute("0.0.0.0", 0);

        String address6 = extra(intent, "address6", "");
        if (!address6.isEmpty()) {
            builder.addAddress(address6, intent.getIntExtra("prefix6", 128));
            builder.addRoute("::", 0);
        }

        for (String server : extra(intent, "dns", "").split(",")) {
            if (!server.isEmpty()) {
                builder.addDnsServer(server);
            }
        }

        // Everything this process sends — the proxy's own upstream connections included — must
        // bypass the tun, or each proxied connection would be captured and fed back into itself.
        try {
            builder.addDisallowedApplication(getPackageName());
        } catch (Exception e) {
            Log.e(TAG, "could not exclude self from the VPN: " + e);
            nativeVpnFailed("could not exclude Privaxy's own traffic from the VPN");
            return;
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            builder.setMetered(false);
        }
        builder.setBlocking(false);
        builder.setConfigureIntent(launchIntent());

        ParcelFileDescriptor pfd;
        try {
            pfd = builder.establish();
        } catch (Exception e) {
            Log.e(TAG, "establish failed: " + e);
            nativeVpnFailed("could not establish the VPN interface: " + e);
            return;
        }
        if (pfd == null) {
            // Null means consent was revoked, or another app took the VPN slot.
            nativeVpnFailed("the VPN interface was refused; another VPN may be active");
            return;
        }

        tunOpen = true;
        try {
            nativeVpnStarted(pfd.detachFd(), mtu);
        } catch (Throwable t) {
            // The native library is only loaded once the activity has run.
            Log.e(TAG, "nativeVpnStarted unavailable: " + t);
            tunOpen = false;
            stopSelf();
        }
    }

    /** Signal Rust to close the tun; the interface goes away with the descriptor. */
    private void teardown() {
        if (!tunOpen) {
            return;
        }
        tunOpen = false;
        try {
            nativeVpnStopped();
        } catch (Throwable t) {
            Log.e(TAG, "nativeVpnStopped unavailable: " + t);
        }
    }

    @Override
    public void onRevoke() {
        // The user turned the VPN off in Settings, or another app claimed the slot.
        teardown();
        stopSelf();
        super.onRevoke();
    }

    @Override
    public void onDestroy() {
        teardown();
        super.onDestroy();
    }

    private boolean goForeground(String title, String text) {
        NotificationManager manager =
                (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
        // IMPORTANCE_LOW: an ongoing status row, not something that should buzz.
        NotificationChannel channel =
                new NotificationChannel(CHANNEL_ID, "VPN", NotificationManager.IMPORTANCE_LOW);
        channel.setShowBadge(false);
        manager.createNotificationChannel(channel);

        Intent stop = new Intent(this, EguiVpnService.class).setAction(ACTION_STOP);
        PendingIntent stopIntent =
                PendingIntent.getService(
                        this, 0, stop, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        // An app with no android:icon has icon 0, and a foreground notification without a small
        // icon is not merely ignored: the system answers startForeground with
        // BadForegroundServiceNotificationException, which is delivered asynchronously and kills
        // the process — past any try/catch here.
        int icon = getApplicationInfo().icon;
        if (icon == 0) {
            icon = android.R.drawable.sym_def_app_icon;
        }

        Notification notification =
                new Notification.Builder(this, CHANNEL_ID)
                        .setContentTitle(title)
                        .setContentText(text)
                        .setSmallIcon(icon)
                        .setContentIntent(launchIntent())
                        .setOngoing(true)
                        .setShowWhen(false)
                        .setOnlyAlertOnce(true)
                        .addAction(new Notification.Action.Builder(null, "Stop", stopIntent).build())
                        .build();

        try {
            if (Build.VERSION.SDK_INT >= SDK_UPSIDE_DOWN_CAKE) {
                startForeground(
                        NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
            } else {
                startForeground(NOTIFICATION_ID, notification);
            }
            return true;
        } catch (Exception e) {
            Log.e(TAG, "startForeground failed: " + e);
            nativeVpnFailed("Android refused the foreground service: " + e);
            return false;
        }
    }

    /** Tapping the notification reopens the app. */
    private PendingIntent launchIntent() {
        Intent launch = getPackageManager().getLaunchIntentForPackage(getPackageName());
        if (launch == null) {
            launch = new Intent();
        }
        return PendingIntent.getActivity(
                this, 0, launch, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    private static String extra(Intent intent, String key, String fallback) {
        String value = intent != null ? intent.getStringExtra(key) : null;
        return value != null ? value : fallback;
    }

    /** Implemented in Rust (egui-android `vpn`); registered by `vpn::register_natives`. */
    private static native void nativeVpnStarted(int fd, int mtu);

    private static native void nativeVpnStopped();

    private static native void nativeVpnFailed(String reason);
}
