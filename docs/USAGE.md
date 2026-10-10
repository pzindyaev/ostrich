# Usage

A guide to the Ostrich dashboard, pane by pane and form by form. Installation, the on-disk layout, `vm.yaml`, networking, firmware and the other reference material live in the [README](../README.md).

## The dashboard

Everything happens on one screen. The VMs and the templates are listed on the left; the right side follows whatever is selected: the VM's details on top and its serial console below. Forms and device dialogs open in the right-hand column, so the lists stay in view; confirmations, long errors and the key reference are centred popups.

```
╭ Virtual Machines (3) ──────────╮╭ debian-12 ─────────────────────────────────────────────────────╮
│ ▸ debian-12   ● running        ││ Status    ● running  (PID 767371)                              │
│   ubuntu-24   ● stopped        ││ CPU       2 cores                                              │
│   windows-11  ● stopped        ││ RAM       2048 MiB                                             │
│                                ││ Disk      20 GiB                                               │
│                                ││           data                   50 GiB  ● 4.3 GiB on host     │
│                                ││ ISO       (none)                                               │
│                                ││ Firmware  UEFI, TPM 2.0                                        │
│                                ││ Net       user [2222→22]                                       │
│                                ││ IP        10.0.2.15  —  ssh -p 2222 localhost                  │
│                                ││ VNC       127.0.0.1:1  (TCP port 5901)                         │
│                                ││ USB       C922 Pro Stream Webcam  046d:085c  ● connected       │
│                                ││ USB ISO   (none)                                               │
│                                ││ Created   2026-03-17 10:00                                     │
│                                │╰────────────────────────────────────────────────────────────────╯
│                                │╭ Serial console · debian-12 · last 200 lines · 2s ──────────────╮
│                                ││ [  OK  ] Started ssh.service - OpenBSD Secure Shell server.    │
╰────────────────────────────────╯│                                                                │
╭ Templates (2) ─────────────────╮│ Debian GNU/Linux 12 debian ttyS0                               │
│ ▸ debian-12-base  UEFI, TPM…   ││                                                                █
│   win11-base      UEFI + Secu… ││ debian login:                                                  █
╰────────────────────────────────╯╰─────────────────────────────────────────────────── ↓ following ╯
 ✓ started debian-12
 j/k move  s start  x stop  n new  e edit  u USB  i ISO  t template  d delete  ? keys  q quit
```

Three panes take the keyboard: the **VM list**, the **templates list** and the **console**. `Tab` and `Shift-Tab` cycle through them; the focused pane has the highlighted border. The two lines at the bottom are the status bar (the result of the last action) and the keys that apply right now. When the terminal is too narrow for every key hint, whole hints are left out, the least useful first (above: `Tab`, `Enter`, `c`, `v` and `r`); `? keys` and `q quit` always stay, and in a form or dialog so does its `Esc` hint, or `Ctrl-c quit` while its work runs. `?` opens the full key reference at any time.

The left column takes a third of the terminal's width (30–54 columns), and below 90 columns 40 % of it (24–30 columns); the details card gets the rows it needs, up to 70 % of the right column, and the console the rest. Below 40 columns or 9 rows the screen only says `terminal too small`; at 9 rows every pane still shows a row.

Lists and statuses refresh every 2 seconds, as does the console. A VM that was started from a second terminal, or that shut itself down, shows up without any key press.

### VM list

| Key | Action |
|-----|--------|
| `j` / `↓`, `k` / `↑` | Move the cursor |
| `g` / `G` | Jump to top / bottom |
| `Ctrl-d` / `Ctrl-u` | Half-page down / up |
| `Enter` / `l` / `→` | Focus the console of the selected VM |
| `Tab` / `Shift-Tab` | Focus the next / previous pane |
| `T` | Focus the templates pane |
| `n` | Create a new VM |
| `e` | Edit the selected VM |
| `u` | USB passthrough for the selected VM |
| `i` | ISO hot-plug for the selected VM |
| `t` | Save the selected VM as a template (it must be stopped) |
| `s` | Start the selected VM |
| `x` | Stop the selected VM |
| `c` | Connect to the serial console interactively (requires `socat`) |
| `v` | Launch a VNC viewer (requires VNC enabled and a viewer installed) |
| `d` | Delete the selected VM (asks first) |
| `r` | Refresh now |
| `Esc` | Clear the status bar message |
| `?` | Key reference |
| `q` / `Ctrl-c` | Quit — VMs keep running; see [Quitting](#quitting) |

Each row shows the VM's name, `● running` or `● stopped`, and, where the pane is wide enough, its CPU, RAM and main disk size with ` +n` for additional disks.

With no VMs yet the pane says `No VMs yet. Press n to create one.` and the hints offer only the keys that work without a VM: `n new  Tab pane  r refresh  ? keys  q quit` (on the console pane `h back  Tab pane  r refresh  ? keys  q quit`). A narrow bar drops `r refresh` first, then `Tab pane`.

One start, stop or delete runs at a time: while one is in flight (the spinner on the right of the status bar), `s`, `x` and `d` start nothing new. `s` on a running VM says `✗ VM "debian-12" is already running (PID 767371)`, `x` on a stopped one `✗ ubuntu-24 is not running`.

### Details pane

The card at the top right describes the selected VM: status with the PID while it runs, CPU, RAM, the main disk followed by one line per additional disk (its size and whether its image is on the host), the boot ISO with its size or `✗ not found`, firmware, network type with its port forwards, the guest's IP address (see [Network modes](../README.md#network-modes)) with a ready `ssh -p <port> localhost` when a TCP port forward goes to guest port 22, VNC, the USB devices passed through with `● connected`, `✗ no access` or `○ not connected`, the images attached as USB drives, and when the VM was created. A `…` stands for a value the 2-second refresh has not delivered yet.

Rows never wrap. A value too wide for the card ends in `…`, cut at a word boundary; a path is cut from the left instead (`…/debian-12.3.0-amd64-netinst.iso  ● 631 MiB`), so the file name and its state stay in view. On a narrow card the IP row drops `gw 10.0.2.2`, then `(DHCP, guest-internal)`, but keeps the `ssh` command. The additional disks, the boot ISO and the USB images line their states up in one column, and the USB devices line theirs up by the longest name. A card too short for all its rows ends with `… n more — enlarge the terminal`.

When the templates pane is focused the card describes the selected template instead (see [Templates](#templates)).

### Console pane

The serial console pane always shows the last 200 lines of the selected VM's `console.log`, refreshed every 2 seconds — no connection needed. Only the end of the log is read, and a line longer than 64 KiB shows only its last 64 KiB. It follows the newest output (`↓ following` in the corner) until you scroll up (`↑ scrolled`); `G` goes back to the tail and follows again. Lines are hard-wrapped to the pane; terminal escape sequences are dropped (colours with them), and carriage returns, backspaces and tabs are applied the way a terminal would show them. On a narrow terminal the title gives up ` · 2s` first, then ` · last 200 lines`, then cuts the VM name; a pane too narrow even for ` Serial console ` cuts that with an ellipsis (` Serial cons… `).

Press `Enter` or `l` on the VM list to give the console the keyboard:

| Key | Action |
|-----|--------|
| `j` / `↓`, `k` / `↑` | Scroll one line |
| `Ctrl-d` / `Ctrl-u` | Half-page scroll |
| `Ctrl-f` / `Ctrl-b`, `PageDown` / `PageUp` | Full-page scroll |
| `g` / `G`, `Home` / `End` | Top / bottom (and follow again) |
| `s` `x` `e` `u` `i` `t` `c` `v` `d` | Act on the selected VM, as on the VM list |
| `r` | Refresh now |
| `h` / `←` / `Esc` | Back to the VM list |

`c` suspends the dashboard and drops you into a live, bidirectional session with the VM's serial port through `socat`, with the cursor shown. The terminal is in raw mode meanwhile, so `Ctrl-C`, `Ctrl-Z`, `Ctrl-\`, `Tab` and the arrow keys reach the guest; press `Ctrl-]` to disconnect and return to the dashboard, which says `✓ disconnected from serial console`. On a stopped VM `c` says `✗ VM is not running`; without `socat` installed, `✗ socat not found — install it…` and how. See [Serial Console](../README.md#serial-console).

### Status bar and errors

The line under the panes reports the last action: `✓ started debian-12`, or the error alone when it failed, e.g. `✗ boot ISO: image not found: /home/user/iso/old.iso`. A long error — QEMU's output when it refused to start, the commands that set up tap networking, how to install a missing `socat` or VNC viewer — opens in a popup you can scroll with `j`/`k` (with a scrollbar on its right border when it is longer than the popup) and close with `Esc` or `Enter`, as its footer says: `j/k scroll  Esc/Enter close`; the first line stays in the status bar. While the dashboard's own start, stop or delete is in flight a spinner and what is happening show on the right. Forms and dialogs show their progress in their own pane instead.

Confirmations — deleting a VM or a template — are popups too, with their keys in the footer: `y confirm  any other key cancel`.

### Key reference

`?` opens the key reference. When it is taller than the terminal it scrolls with `j`/`k` or `↓`/`↑`, with a scrollbar on its right border; any other key closes it. On a small terminal it takes most of the width, all of it at 40 columns, and wraps the descriptions and the headings at word boundaries.

```
╭ Keys ───────────────────────────────────────────────────────────────────────────────────╮
│ j/k ↑/↓          move in the focused list or scroll the console                         │
│ g/G              top / bottom                                                           │
│ Ctrl-d/Ctrl-u    half page down / up                                                    │
│ Tab / Shift-Tab  focus the next / previous pane (VMs, templates, console)               │
│ Enter / l        focus the console of the selected VM (VM list)                         │
│ h / Esc          back to the VM list                                                    │
│ T                focus the templates pane                                               │
│ n                new VM (from the selected template when the templates pane is focused) │
│ d                delete the selected VM or template (asks first)                        │
│ r                refresh now                                                            │
│ ?                this help                                                              │
│ q / Ctrl-c       quit (VMs keep running)                                                │
│                                                                                         │
│ The selected VM (VM list and console)                                                   │
│ s / x            start / stop                                                           │
│ e                edit                                                                   │
│ u                USB passthrough                                                        │
│ i                ISO hot-plug                                                           │
│ t                save as a template (it must be stopped)                                │
│ c                connect to the serial console (socat; Ctrl-] to disconnect)            │
│ v                launch a VNC viewer                                                    │
╰──────────────────────────────────────────────────────── j/k scroll  any other key close ╯
```

### Quitting

`q` on the dashboard, or `Ctrl-c` anywhere, quits at once, unless something is still in flight: a start, stop or delete from the dashboard, or a create, save, disk copy or hot-plug in an open form or dialog. Quitting then would cut it short, so a popup says what Ostrich is waiting for and it quits as soon as that is done:

```
╭ Quitting ──────────────────────────────────────────────────────╮
│                                                                │
│ Still stopping debian-12…                                      │
│                                                                │
│ Ostrich quits as soon as that is done.                         │
│                                                                │
╰───────────────────────────────────── Esc stay  Ctrl-c quit now ╯
```

`Esc` or `Enter` calls the quit off (`✓ quit cancelled`) and Ostrich stays; `q` keeps waiting; `Ctrl-c` quits now. The popup always names what is still in flight: when the dashboard's start is done but a form is still saving, it says so. If the work it waits for fails, the quit is called off too: the popup closes, the error shows as it would have anyway, in the status bar, an error popup or the form or dialog it belongs to, and `q` or `Ctrl-c` quits again. While the popup is up, a long error from anything else, a refresh for instance, shows only its first line in the status bar.

A `SIGINT` or `SIGTERM` sent from outside is taken as a `Ctrl-c` (the terminal is restored and Ostrich exits with status 0), on the first-run screen too; while Ostrich waits to quit, a second one quits at once. Closing the terminal (`SIGHUP`) ends Ostrich at once. VMs keep running whichever way Ostrich ends.

## Create VM form

Press `n`. A linear 11-step wizard opens in the right-hand column; the list stays visible on the left. Text input fields accept free typing; the selectors (firmware, TPM, network) use `h/l` or arrow keys.

```
╭ Create VM ──────────────────────────────────────────────────────────────────────╮
│ Step 9 / 11                                                                     │
│                                                                                 │
│ Network type                                                                    │
│ h/l/←/→ to select: user (NAT) · tap (bridge) · none                             │
│                                                                                 │
│ ▸  user (NAT)   tap (bridge)   none                                             │
│                                                                                 │
│ ⚠ tap networking is not set up on this host: the host has no bridge br0.        │
│   Run this, then start the VM:                                                  │
│                                                                                 │
│     sudo nmcli con add type bridge ifname br0 con-name br0 \                    │
│         ipv4.method shared ipv4.addresses 192.168.76.1/24 \                     │
│         ipv6.method disabled bridge.stp no connection.autoconnect yes           │
│     sudo nmcli con up br0                                                       │
│     sudo ufw allow in on br0 to any port 67 proto udp                           │
│     sudo ufw allow in on br0 to any port 53                                     │
│     sudo ufw route allow in on br0                                              │
│                                                                                 │
│   This makes a NAT'd bridge that stays across reboots: guests get               │
│   192.168.76.10–254 by DHCP and reach the internet through the host. The ufw    │
│   rules let the guests use the host's DHCP and DNS and route out.               │
│   For a bridge onto a wired NIC, and to undo, see the README section "Setting   │
│   up br0".                                                                      │
╰─────────────────────────────────────────────────────────────────────────────────╯
```

| Step | Field | Notes |
|------|-------|-------|
| 1 | VM Name | Letters, digits, hyphens, underscores |
| 2 | CPU Cores | Positive integer up to 4294967295, e.g. `2` |
| 3 | RAM (MiB) | 64 to 4294967295, e.g. `2048` for 2 GiB |
| 4 | Disk Size (GiB) | 1 to 4294967295, e.g. `20` |
| 5 | Additional disks | Optional. Comma-separated `[name:]size` in GiB, e.g. `data:50, 100` — see [Additional Disks](../README.md#additional-disks) |
| 6 | Boot ISO | Optional. Pick an image used before or type the path of a new one — see [Choosing an ISO](#choosing-an-iso) |
| 7 | Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot` — see [UEFI, Secure Boot and TPM 2.0](../README.md#uefi-secure-boot-and-tpm-20) |
| 8 | TPM 2.0 | `disabled` · `enabled` (needs `swtpm`) |
| 9 | Network type | `user (NAT)` · `tap (bridge)` · `none`. Picking `tap` on a host without the bridge shows what is missing and the commands that set it up — see [Setting up `br0`](../README.md#setting-up-br0) |
| 10 | VNC Display Number | `0` to disable; `1`–`99` enables VNC on TCP port `5900+N` |
| 11 | Confirm | Review and submit. A summary row too wide for the pane is cut by display width and ends in `…` after a whole word; the ISO row is cut from the left, so the file name stays |

A value that does not pass is refused on its own step, with the reason under the input (`CPU must be a positive integer`, `RAM must be at least 64 MiB`, `disk size must be at least 1 GiB` — also for numbers too large), and the wizard stays there until it is fixed.

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field (`j` too on the selector and confirm steps) |
| `Shift-Tab` / `↑` | Previous field (`k` too on the selector and confirm steps) |
| `h` / `l` / `←` / `→` | Cycle a selector (steps 7–9) |
| `Esc` | Cancel and return to the dashboard |

On the Boot ISO step the arrow keys move within the [ISO dialog](#choosing-an-iso) instead: `Enter` takes the row under the cursor and moves on, `Tab` moves on with the choice as it is (a path typed on the *New path* row but not entered yet is taken too), `Shift-Tab` goes back.

While the VM is being created the form shows `⠋ Creating debian-12…`, takes no keys and its hints read `Ctrl-c quit`; quitting waits for it. When the VM is created the form closes, the new VM is selected in the list and the status bar says so.

## Edit VM form

Press `e` on the selected VM. All properties are shown on one page, pre-filled with the current values from its `vm.yaml`; whether the VM is running is read as the form opens, too. The selectors start in the same column as the text values; in a narrow pane a selector wraps under its first choice, so the chosen value stays in view.

```
╭ Edit VM: debian-12 ─────────────────────────────────────────────────────────────╮
│ ● running — changes take effect on next start; name, disk sizes, disk removal   │
│   and firmware are locked; new disks are hot-plugged                            │
│                                                                                 │
│   Name             debian-12                                                    │
│   CPU Cores        2                                                            │
│   RAM (MiB)        2048                                                         │
│   Disk Size (GiB)  20                                                           │
│ ▸ Extra Disks      data:50                                                      │
│   Boot ISO         (none)                                                       │
│   Firmware         BIOS   UEFI   UEFI + Secure Boot                             │
│   TPM 2.0          disabled   enabled                                           │
│   Network          user (NAT)   tap (bridge)   none                             │
│   MAC Address      52:54:00:ab:cd:ef                                            │
│   Port Forwards    tcp:2222:22                                                  │
│   VNC Display      1                                                            │
│                                                                                 │
│    Save                                                                         │
│                                                                                 │
│   Comma-separated [name:]size in GiB, e.g. data:50, scratch:10. A new disk is   │
│   hot-plugged into a running VM and arrives blank: partition and format it in   │
│   the guest. Grow or remove only when stopped; removing deletes the image       │
╰─────────────────────────────────────────────────────────────────────────────────╯
```

| Field | Notes |
|-------|-------|
| Name | Renames the VM directory; VM must be stopped |
| CPU Cores / RAM (MiB) | Same rules as the create form |
| Disk Size (GiB) | Can only grow (`qemu-img resize`); VM must be stopped. The guest still has to extend its own partitions/filesystem |
| Extra Disks | Comma-separated `[name:]size` in GiB, e.g. `data:50, scratch:10`. A new disk is created and, on a running VM, hot-plugged right away; it arrives blank, so partition and format it in the guest. Growing or removing one needs the VM stopped. Removing a disk **deletes its image** once a second `Ctrl-s` confirms — see [Additional Disks](../README.md#additional-disks) |
| Boot ISO | `Enter` (or `l`) opens the [ISO dialog](#choosing-an-iso): an image used before, the path of a new one, or `(none)` to boot from disk (e.g. after installation). A running VM gets the new disc right away |
| Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot`; VM must be stopped. Turning Secure Boot on rebuilds the VM's UEFI NVRAM |
| TPM 2.0 | `disabled` · `enabled`; needs `swtpm` on the host |
| Network | `user (NAT)` · `tap (bridge)` · `none` |
| MAC Address | Six colon- or hyphen-separated pairs of hex digits, e.g. `52:54:00:ab:cd:ef`, or three dot-separated groups of four (`5254.00ab.cdef`); QEMU is given the colon form either way. Leave blank to generate a new random one |
| Port Forwards | `user` mode only. Comma-separated `[tcp\|udp:]host:guest`, e.g. `2222:22, udp:5353:53` |
| VNC Display | `0` to disable; `1`–`99` |

Changes to a running VM are saved but only take effect the next time it is started, except the boot ISO and newly added disks, which are applied in the guest right away. If applying one of those fails, the config is saved anyway and the error says so. A value that does not pass is named under the form and the cursor goes to its field; that error, and the warning before disks are removed, stay in view on a small terminal, where the field's help is left out first. While the form saves, it takes no keys and its hints read `Ctrl-c quit`.

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field (`j` too on a selector, Boot ISO and **Save**) |
| `Shift-Tab` / `↑` | Previous field (`k` too on a selector, Boot ISO and **Save**) |
| `h` / `l` / `←` / `→` | Cycle a selector (firmware, TPM, network) |
| `Ctrl-s` (or `Enter` on **Save**) | Save changes |
| `Esc` | Cancel and return to the dashboard |

## USB passthrough dialog

Press `u` on the selected VM. It lists the USB devices connected to the host (hubs are left out — they stay with the host kernel) and marks the ones passed through to this VM. Devices that are configured but currently unplugged are listed at the bottom as *not connected*. Whether the VM is running is read as the dialog opens.

```
╭ USB Passthrough: debian-12 ─────────────────────────────────────────────────────╮
│ ● running — attaching or detaching hot-plugs the device in the guest            │
│ The host cannot use a device while the guest holds it.                          │
│                                                                                 │
│ Host USB devices   [x] = passed through to this VM                              │
│                                                                                 │
│   [ ] 0b05:18f3  AsusTek Computer Inc. AURA LED …  port 1-5.3    ✗ no access    │
│   [ ] 2972:0077  FiiO K11                          port 3-2.2.1  ✗ no access    │
│   [x] 046d:085c  C922 Pro Stream Webcam            port 3-2.2.2                 │
│ ▸ [ ] 046d:c52b  Logitech USB Receiver             port 3-2.2.3  ✗ no access    │
│   [ ] 4d64:5277  guojing.io Lily58 Wireless        port 3-2.2.4  ✗ no access    │
│   [ ] 046d:c24a  Logitech Gaming Mouse G600        port 3-2.3    ✗ no access    │
│   [ ] 0a12:0001  CSR8510 A10                       port 5-2      ✗ no access    │
│   [x] 0781:5583  SanDisk Ultra Fit                 not connected                │
│                                                                                 │
│ ✗ no write access to /dev/bus/usb/003/007 — QEMU cannot open it                 │
│ Run this to grant access to your user (y copies it), then press r to rescan:    │
│     echo 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",' \                        │
│     'ATTR{idProduct}=="c52b",' 'TAG+="uaccess"' | sudo tee -a \                 │
│     /etc/udev/rules.d/70-ostrich-usb.rules && sudo udevadm control \            │
│     --reload && sudo udevadm trigger                                            │
│                                                                                 │
╰─────────────────────────────────────────────────────────────────────────────────╯
 Space/Enter attach/detach  y copy udev command  a add by ID  r rescan  j/k move  q/Esc back
```

| Key | Action |
|-----|--------|
| `Space` / `Enter` | Attach or detach the device under the cursor |
| `a` | Add a device by `vendor:product` ID (as printed by `lsusb`), e.g. one that is not plugged in yet; `Enter` attaches it, `Esc` cancels |
| `y` | Copy the udev command that grants access to the device under the cursor (shown when it is marked **✗ no access**); the clipboard tools it uses are listed under [Host permissions](../README.md#host-permissions) |
| `r` | Rescan host devices |
| `j` / `k`, `g` / `G` | Move cursor |
| `q` / `h` / `Esc` | Back to the dashboard (not while a hot-plug is in flight) |

Every toggle is written to `vm.yaml` immediately. If the VM is running the device is hot-plugged (or unplugged) through the QEMU monitor on the spot; otherwise it is attached the next time the VM starts. Move the cursor onto a device marked **✗ no access** and the dialog shows the command that grants access, ready to select with the mouse or copy with `y`. See [USB Passthrough](../README.md#usb-passthrough) for the host-side permissions this needs.

One change runs at a time: while a hot-plug is in flight the dialog shows a spinner, `Space` and `Enter` do nothing, and an ID typed after `a` stays in its field until `Enter` can take it. The dialog stays open until the change is done: `q`, `h` and `Esc` do nothing meanwhile, and the last hint reads `Ctrl-c quit` instead of `q/Esc back`. The `✗ no access` markers line up in their own column. In a narrow pane the name column shrinks first, and the banners and help wrap. When the devices do not all fit, the list scrolls with the cursor and a line under it says which part is shown (`↓ 1–6 of 8`); in a short pane the blank spacer lines go first, so the udev command stays in view.

## ISO hot-plug dialog

Press `i` on the selected VM. The top shows the boot ISO in the VM's CD-ROM drive, which you can swap or eject; below it are the disk images attached to the VM as USB drives, which you can add to or detach from. On a running VM every change is applied in the guest on the spot, through the QEMU monitor; on a stopped VM it takes effect at the next start. Either way it is written to `vm.yaml` immediately. Whether the VM is running is read as the dialog opens.

```
╭ ISO Hot-plug: debian-12 ────────────────────────────────────────────────────────╮
│ ● running — changes are applied in the guest right away                         │
│                                                                                 │
│ Boot ISO (CD-ROM drive)                                                         │
│                                                                                 │
│   /home/user/iso/debian-12.3.0-amd64-netinst.iso  ● 631 MiB                     │
│                                                                                 │
│ USB drives   images attached read-only; the guest sees each one as a USB stick  │
│                                                                                 │
│   /home/user/iso/old-drivers.iso                  ✗ not found                   │
│ ▸ /home/user/iso/virtio-win.iso                   ● 611 MiB                     │
│                                                                                 │
│ ✓ attached virtio-win.iso (hot-plugged as a USB drive)                          │
╰─────────────────────────────────────────────────────────────────────────────────╯
 c change boot ISO  e eject  a attach USB image  Space/Enter/d detach  r refresh  j/k move  q/Esc back
```

| Key | Action |
|-----|--------|
| `c` | Put a different ISO in the CD-ROM drive — opens the [ISO dialog](#choosing-an-iso) |
| `e` | Eject the boot ISO, leaving the drive empty |
| `a` | Attach an image as a USB drive — opens the [ISO dialog](#choosing-an-iso) |
| `Space` / `Enter` / `d` | Detach the USB image under the cursor |
| `r` | Re-check the image files |
| `j` / `k`, `g` / `G` | Move cursor |
| `q` / `h` / `Esc` | Back to the dashboard (not while a change is in flight) |

The line at the bottom reports the last change, or a spinner while one is in flight; it stays in view whatever the pane's height. One change runs at a time: while one is in flight, `e` and detaching do nothing, and picking an image says `still <what> — press Enter again when that is done`. The dialog stays open until the change is done: `q`, `h` and `Esc` do nothing meanwhile, and the last hint reads `Ctrl-c quit` instead of `q/Esc back`. The boot ISO and the USB drives share one column for their states. When the USB drives do not all fit, the list scrolls with the cursor and a line under it says which part is shown (`↑↓ 3–4 of 7`).

**Boot ISO.** The CD-ROM drive is always part of the VM, empty when no ISO is configured, so a disc can go in at any time. It sits where QEMU's `-cdrom` puts it, so guests see the same hardware as before. Swapping forces the tray open first (`eject -f cdrom`, then `change cdrom <path> raw`), so a guest that has locked it, as Linux does while the disc is mounted, cannot hold up the swap; it sees the disc change the way it would with a real drive. The edit form's Boot ISO field does the same when the VM is running. A VM whose boot ISO has gone missing refuses to start, names the file and says what to do: `Eject it in the ISO dialog (i), clear it in the edit form (e), or put the file back.`

**USB drives.** Images are attached read-only, so the file is never modified and several VMs can share one; the guest sees a removable USB stick holding the image byte for byte. Each image is a drive plus a `usb-storage` device on the VM's xHCI controller. A VM whose image file has gone missing refuses to start, too: `USB drive old-drivers.iso: image not found: /home/user/iso/old-drivers.iso`, then `Detach it in the ISO dialog (i), or put the file back.`

```
-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/home/user/iso/debian-12.3.0-amd64-netinst.iso
-drive if=none,id=usbimg-virtio-win.iso-drive,format=raw,readonly=on,file=/home/user/iso/virtio-win.iso
-device usb-storage,id=usbimg-virtio-win.iso,bus=xhci.0,drive=usbimg-virtio-win.iso-drive,removable=on
```

What the guest makes of a USB image depends on the image. A Linux guest mounts the ISO9660 filesystem straight off the stick (`mount /dev/sdb /mnt`), and a fresh VM with an empty disk boots a hybrid ISO — most Linux installers — from it under both BIOS and UEFI. Windows does not mount ISO9660 from a disk-class device, so a plain ISO shows up there as an unformatted drive; for Windows, put the ISO in the CD-ROM drive instead. Any raw disk image works as a USB drive, not just `.iso` files.

## Choosing an ISO

Every place that takes an image — the Boot ISO step of the create form, the Boot ISO field of the edit form, and `c` and `a` on the ISO hot-plug dialog — opens the same dialog. In the create form it is the step itself; elsewhere it is a popup over the screen, with its keys on its bottom border (`Enter pick` in the edit form, `Enter insert` for the CD-ROM drive, `Enter attach` for a USB drive). It lists the images used before, each with its size or `✗ not found` when the file has gone, and the VMs that have it right now (cut short with `…` when they do not fit); the last row takes the path of a new one. In the forms a first row, `(none)`, stands for no boot ISO.

```
╭ Boot ISO (CD-ROM drive) ─────────────────────────────────────────────────────────────────────────╮
│ the ISO to put in the drive: one used before, or the path of a new one; ~ is your home directory │
│                                                                                                  │
│ ▸ /home/user/iso/virtio-win.iso                      ● 611 MiB  in use by debian-12, windows-11  │
│   /home/user/iso/debian-12.3.0-amd64-netinst.iso     ● 631 MiB  in use by debian-12              │
│   /home/user/iso/ubuntu-24.04-live-server-amd64.iso  ✗ not found                                 │
│   /home/user/iso/old-drivers.iso                     ✗ not found  in use by debian-12            │
│   New path  /path/to/image.iso                                                                   │
╰──────────────────────────────────────────────────── Enter insert  ↑/↓ move  d forget  Esc cancel ╯
```

| Key | Action |
|-----|--------|
| `Enter` | Take the row under the cursor: the image, the path typed on *New path* (`~` is your home directory), or `(none)` |
| `↑` / `↓`, `Tab` / `Shift-Tab` | Move the cursor; `j` / `k`, `g` / `G` work too, except on the *New path* row, where letters type |
| `d` | Forget the image under the cursor. One that a VM still has stays listed until it is ejected or detached there |
| `Esc` | Close the dialog without changing anything (on the create form's step it cancels the wizard) |

The dialog accepts only a path that names a readable file, and says so under the list otherwise (a long error wraps), so a typo cannot get into a VM. In the forms the cursor starts on the image the field holds (on *New path*, with the path filled in, when it is not in the list), or on `(none)`; on the ISO hot-plug dialog it starts on the first image, or on *New path* when nothing was used before.

The list is kept in `recent_isos` of the [app config](../README.md#app-configuration): every image put in a CD-ROM drive or attached as a USB drive goes to the top (which is why `virtio-win.iso`, attached last, leads the list above), the newest 20 are kept. Images a VM has in its `vm.yaml` are offered as well, whether or not they are in that list, so the dialog is full from the first use.

## Templates

A template is a stopped VM frozen as a starting point for new VMs: a copy of its disk image together with the machine definition the installed OS depends on (architecture, firmware, Secure Boot, TPM), its UEFI NVRAM (the boot entries) and its TPM state. Install and configure an OS once, save it as a template, and every VM made from it boots straight into that system with its own name, CPU, RAM, network and MAC address.

Templates are full copies: deleting the source VM, or the template, does not affect VMs made from it. They live in `.templates/` under the VM storage directory — see [VM Storage Layout](../README.md#vm-storage-layout).

### Save as template

Press `t` on the selected VM. **The VM must be stopped** — shut it down from inside the guest first; whether it runs is read as the form opens. Ostrich refuses a running VM rather than stopping it, because stopping QEMU kills the machine without a guest shutdown and would leave the filesystem in the copy dirty, which is the opposite of what a template is for.

```
╭ Save as Template: debian-12 ────────────────────────────────────────────────────╮
│ ● stopped — the disk is in a consistent state and can be copied                 │
│                                                                                 │
│ What goes into the template                                                     │
│ ╭─────────────────────────────────────────────────────────────────────────────╮ │
│ │ Disk:     20 GiB virtual, 4.3 GiB on the host — copied in full              │ │
│ │ Firmware: UEFI, TPM 2.0 — with the UEFI NVRAM (boot entries) and the TPM    │ │
│ │           state                                                             │ │
│ │ Defaults: 2 cores, 2048 MiB RAM, user network — chosen anew for each VM     │ │
│ │           made from it                                                      │ │
│ ╰─────────────────────────────────────────────────────────────────────────────╯ │
│ Left out, as they belong to one VM: MAC address, port forwards, VNC display,    │
│ boot ISO, USB devices and images, additional disks.                             │
│                                                                                 │
│ ▸ Template name    debian-12-docker                                             │
│   Description      Debian 12 with docker and my dotfiles                        │
│                                                                                 │
│    Save template                                                                │
╰─────────────────────────────────────────────────────────────────────────────────╯
 Tab/↓ next  Shift-Tab/↑ back  Ctrl-s save  Esc cancel
```

| Field | Notes |
|-------|-------|
| Template name | Letters, digits, hyphens and underscores; defaults to the VM's name |
| Description | Optional, shown in the template's card |

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field |
| `Shift-Tab` / `↑` | Previous field |
| `Ctrl-s` (or `Enter` on **Save template**) | Save |
| `Esc` | Cancel and return to the dashboard |

The disk image is copied with `qemu-img convert`, which writes a fresh, compact qcow2 holding only the allocated clusters; the virtual size stays the same. This takes a while for a large disk — the form shows a spinner meanwhile, its hints read `Ctrl-c quit`, and the dashboard stays responsive (quitting waits for the copy, see [Quitting](#quitting)). Nothing half-made is left behind if the copy fails; the error says why, e.g. `copy disk image: exit status 1`. When it is done the templates pane takes the focus with the new template selected.

What a template carries, and what it does not:

| Carried | Left out (belongs to one VM or to the host) |
|---------|---------------------------------------------|
| Disk image | MAC address (a new one is generated) |
| UEFI NVRAM — boot entries, Secure Boot keys | Port forwards (two VMs cannot share a host port) |
| TPM state — so BitLocker and Windows Hello still work | VNC display number (two VMs cannot share a port; a new VM gets a free one if the source had VNC) |
| Architecture, firmware, Secure Boot, TPM | Boot ISO (a clone would run the installer again) |
| CPU, RAM and network type, as defaults | USB devices and USB images |
| | Additional disks (they hold one VM's data, not the installed system) |

### Templates pane

Press `T`, or `Tab` from the VM list, to focus the templates pane at the bottom left. Each row shows the template's name and the machine it defines, cut with `…` to fit; the card on the right describes the one under the cursor, while the console pane keeps showing the selected VM. On a narrow card the Defaults row leaves out `— chosen anew for each VM`, as above, and the VNC row says just `enabled`.

```
╭ Virtual Machines (3) ──────────╮╭ Template ──────────────────────────────────────────────────────╮
│ ▸ debian-12   ● running        ││ Template  win11-base                                           │
│   ubuntu-24   ● stopped        ││ About     Windows 11 23H2, updates applied, virtio drivers     │
│   windows-11  ● stopped        ││ From VM   windows-11, saved 2026-10-09 13:33                   │
│                                ││ Disk      64 GiB virtual, 18.2 GiB on the host                 │
│                                ││ Firmware  UEFI + Secure Boot, TPM 2.0                          │
│                                ││ Defaults  4 cores, 8192 MiB RAM, user network                  │
│                                ││ VNC       enabled — a new VM gets a free display number        │
│                                │╰────────────────────────────────────────────────────────────────╯
╰────────────────────────────────╯╭ Serial console · debian-12 · last 200 lines · 2s ──────────────╮
╭ Templates (2) ─────────────────╮│ Debian GNU/Linux 12 debian ttyS0                               │
│   debian-12-base  UEFI, TPM…   ││                                                                █
│ ▸ win11-base      UEFI + Secu… ││ debian login:                                                  █
╰────────────────────────────────╯╰─────────────────────────────────────────────────── ↓ following ╯
 ✓ started debian-12
 j/k move  Enter/n new VM from template  d delete  h back  Tab pane  ? keys  q quit
```

| Key | Action |
|-----|--------|
| `Enter` / `l` / `→` / `n` | Create a new VM from the template under the cursor |
| `d` | Delete the template (asks first; VMs made from it are not affected). Ignored while a start, stop or delete is in flight |
| `j` / `k`, `g` / `G` | Move cursor |
| `h` / `←` / `Esc` | Back to the VM list |

With no templates yet the pane says `No templates yet — t saves a stopped VM as one.` (shorter on a small pane), `Enter`, `l` and `n` open the plain [create form](#create-vm-form), and the hints read `Enter/n new VM  h back  Tab pane  ? keys  q quit`. If the templates directory cannot be read, the error shows in this pane; the VM list is not affected.

### New VM from template

A 6-step wizard, pre-filled with the template's defaults. The disk, architecture, firmware, Secure Boot and TPM come from the template and are not asked for.

| Step | Field | Notes |
|------|-------|-------|
| 1 | VM Name | Defaults to `<template>-1`, `-2`, … whichever is free |
| 2 | CPU Cores | Positive integer up to 4294967295 |
| 3 | RAM (MiB) | 64 to 4294967295 |
| 4 | Network type | `user (NAT)` · `tap (bridge)` · `none` |
| 5 | Port forwards | `user` mode only; blank for none. Pick host ports no other VM uses |
| 6 | Confirm | Review and create; a row too wide for the pane is cut as on the create form's Confirm step |

```
╭ New VM from Template: win11-base ───────────────────────────────────────────────╮
│ Step 6 / 6   —   64 GiB disk, UEFI + Secure Boot, TPM 2.0: from the template    │
│                                                                                 │
│ Confirm                                                                         │
│ Press Enter to create the VM. The disk is copied from the template, which takes │
│ a while for a large disk                                                        │
│                                                                                 │
│ ╭───────────────────────────────────────────────────────╮                       │
│ │ Name:     win11-test                                  │                       │
│ │ Template: win11-base                                  │                       │
│ │ CPU:      2 cores                                     │                       │
│ │ RAM:      4096 MiB                                    │                       │
│ │ Disk:     64 GiB (copied from the template)           │                       │
│ │ Firmware: UEFI + Secure Boot, TPM 2.0                 │                       │
│ │ Net:      user [tcp:3390:3389]                        │                       │
│ │ VNC:      display 3 (port 5903) — the lowest one free │                       │
│ ╰───────────────────────────────────────────────────────╯                       │
╰─────────────────────────────────────────────────────────────────────────────────╯
 Enter/j create VM  k/Shift-Tab back  Esc cancel
```

The keys and the checks are those of the [create form](#create-vm-form). While the disk is copied the form shows a spinner and its hints read `Ctrl-c quit`; quitting waits for the copy. The new VM gets a fresh MAC address and, when the template's source VM had a VNC display, the lowest display number no existing VM uses; change either later in the edit form. Everything else about the VM — ISO, USB, port forwards, VNC — is edited the same way as for any other VM.

## First run

On the first launch, before the dashboard, a setup screen asks for the **VM storage directory**. All VM sub-folders are created there. The choice is saved to `~/.config/ostrich/config.json` and never asked again; the directory is created if it does not exist. A leading `~/` is your home directory. The screen also comes up when `config.json` has no `vm_storage_path`, or a `null` or blank one, instead of the VMs going into whatever directory Ostrich was started from; the image paths the file remembers are kept when the new path is saved.

`Enter` confirms; a problem (`path cannot be empty`, `cannot create directory: …`, `cannot save config: …`) shows under the input. `Esc` or `Ctrl-c` quits without saving anything, as does a `SIGINT` or `SIGTERM`, and the screen comes back on the next launch.

```
╭────────────────────────────────────────────────────────────────────────────╮
│ Ostrich — QEMU Manager                                                     │
│ First-run setup                                                            │
│                                                                            │
│ VM Storage Directory                                                       │
│ Each VM will get its own sub-folder here containing its disk and config.   │
│                                                                            │
│ ▸ /home/user/VMs                                                           │
│                                                                            │
│ Enter confirm  Esc/Ctrl-c quit                                             │
╰────────────────────────────────────────────────────────────────────────────╯
```
