# The tray icon

Run `rclone-vfsmount-tray` with no subcommand and it puts an icon in your panel.

It needs a panel that speaks **StatusNotifierItem**: KDE Plasma does natively, and GNOME needs
the AppIndicator extension. The old XEmbed system tray is not supported — it does not exist on
Wayland. If nothing is listening the tray waits for a panel to appear rather than exiting, so
starting it before your desktop has finished coming up is fine.

## What the icon says

The tooltip spells out the same thing in words:

| It says | Meaning |
| --- | --- |
| Up to date | Everything that is mounted has finished uploading |
| Uploading | Files are still on their way to the remote |
| Needs attention | A mount failed, an upload errored, or the cache is full. The only state that asks the panel to emphasise the icon — uploads in progress do not |
| State partly unknown | A mount this service manages is up, and what it has left to upload cannot be read: rclone is unreachable, or the mount has no write-back cache to look in. Mounts you started yourself are never counted here — the tray says nothing about those |
| Nothing mounted | No mount is serving |
| Service unreachable | The tray cannot reach the service. **This says nothing about your mounts** — any that were up are still up |
| Connecting… | The tray has only just started, or the service has just restarted, and it has not asked yet |

## The menu

The menu lists every mount with its state. Each opens onto where it is mounted, an **Open** item
that hands the mount point to your file manager, and **Mount** or **Unmount**. A mount that
failed says why. Mounts this service did not start are listed apart and offer only **Open**: the
service will not mount or unmount something it did not start.

What is still to upload is summarised at the top — "3 files, 1.2 GiB pending", a rate, and an
estimate where one can be derived honestly — and again per mount, listing up to ten files. When
a figure comes from reading the cache directory rather than from rclone, the menu says so instead
of passing it off as live progress.

**Quit closes the icon and nothing else.** Your mounts keep serving and so does the service.
Stopping the service is a separate item behind a second click, which says what will happen to
your mounts first.

## When the service is not there

The tray starts whether or not the service is running, and never starts it for you. If the
service is not there the menu says so and offers to start it; if the service is restarted
underneath it, the tray notices and reconnects on its own.

## Inspecting the tray without a panel

The tray needs a panel to be *seen*, not to run: with none, it logs that the icon is not being
shown and keeps waiting. It serves the icon and the menu on the session bus either way, so
`busctl --user list | grep StatusNotifierItem` finds it, and these read what a panel would draw
and click an item:

```sh
N=org.kde.StatusNotifierItem-<pid>-1
busctl --user get-property "$N" /StatusNotifierItem \
    org.kde.StatusNotifierItem IconName Status ToolTip
dbus-send --session --print-reply --dest="$N" /MenuBar \
    com.canonical.dbusmenu.GetLayout int32:0 int32:-1 array:string:label
dbus-send --session --print-reply --dest="$N" /MenuBar \
    com.canonical.dbusmenu.Event int32:<id> string:clicked variant:string:"" uint32:0
```

Read the ids again just before clicking: they change when a mount appears or goes away, and a
click on an old one is refused rather than landing on another item.
