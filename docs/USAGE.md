# Usage

Screen-by-screen guide to the Ostrich TUI. Installation, the on-disk layout, `vm.yaml`, networking, firmware and the other reference material live in the [README](../README.md).

## VM List (main screen)

The main screen lists all VMs with their running status and resource summary.

| Key | Action |
|-----|--------|
| `j` / `↓` | Move cursor down |
| `k` / `↑` | Move cursor up |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `Ctrl-d` | Half-page down |
| `Ctrl-u` | Half-page up |
| `l` / `Enter` | Open VM detail |
| `n` | Create new VM |
| `e` | Edit selected VM |
| `u` | USB passthrough for selected VM |
| `i` | ISO hot-plug for selected VM |
| `t` | Save selected VM as a template (it must be stopped) |
| `T` | Open the templates screen |
| `s` | Start selected VM |
| `x` | Stop selected VM |
| `d` | Delete selected VM (asks confirmation) |
| `r` | Refresh list and statuses |
| `q` | Quit |

## VM Detail screen

Shows configuration, running state and a scrollable view of the serial console output. The console refreshes automatically every 2 seconds.

| Key | Action |
|-----|--------|
| `s` | Start VM (when stopped) |
| `x` | Stop VM (when running) |
| `e` | Edit VM properties |
| `u` | **USB passthrough** — pick host devices for this VM |
| `i` | **ISO hot-plug** — swap the boot ISO, attach disk images as USB drives |
| `t` | **Save as template** — freeze this (stopped) VM as a template for new VMs |
| `c` | **Connect to serial console interactively** (requires `socat`) |
| `v` | **Launch VNC viewer** for this VM (requires VNC enabled and a VNC viewer installed) |
| `j` / `↓` | Scroll console down |
| `k` / `↑` | Scroll console up |
| `Ctrl-d` / `Ctrl-u` | Half-page scroll |
| `Ctrl-f` / `Ctrl-b` | Full-page scroll |
| `g` | Scroll to top |
| `G` | Scroll to bottom |
| `r` | Force console refresh |
| `q` / `h` / `Esc` | Back to VM list |

## Create VM form

A linear 11-step wizard. Text input fields accept free typing; the selectors (firmware, TPM, network) use `h/l` or arrow keys.

| Step | Field | Notes |
|------|-------|-------|
| 1 | VM Name | Letters, digits, hyphens, underscores |
| 2 | CPU Cores | Positive integer, e.g. `2` |
| 3 | RAM (MiB) | Minimum 64, e.g. `2048` for 2 GiB |
| 4 | Disk Size (GiB) | Minimum 1, e.g. `20` |
| 5 | Additional disks | Optional. Comma-separated `[name:]size` in GiB, e.g. `data:50, 100` — see [Additional Disks](../README.md#additional-disks) |
| 6 | Boot ISO | Full path to an `.iso` file, or leave blank |
| 7 | Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot` — see [UEFI, Secure Boot and TPM 2.0](../README.md#uefi-secure-boot-and-tpm-20) |
| 8 | TPM 2.0 | `disabled` · `enabled` (needs `swtpm`) |
| 9 | Network type | `user (NAT)` · `tap (bridge)` · `none` |
| 10 | VNC Display Number | `0` to disable; `1`–`99` enables VNC on TCP port `5900+N` |
| 11 | Confirm | Review and submit |

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `j` / `↓` | Next field |
| `Shift-Tab` / `k` / `↑` | Previous field |
| `h` / `l` / `←` / `→` | Cycle a selector (steps 7–9) |
| `Esc` | Cancel and return to VM list |

## Edit VM form

Press `e` on the list or detail screen to edit an existing VM. All properties are shown on one page, pre-filled with the current values.

| Field | Notes |
|-------|-------|
| Name | Renames the VM directory; VM must be stopped |
| CPU Cores / RAM (MiB) | Same rules as the create form |
| Disk Size (GiB) | Can only grow (`qemu-img resize`); VM must be stopped. The guest still has to extend its own partitions/filesystem |
| Extra Disks | Comma-separated `name:size` in GiB, e.g. `data:50, scratch:10`. A new disk is created and, on a running VM, hot-plugged right away; it arrives blank, so partition and format it in the guest. Growing or removing one needs the VM stopped. Removing a disk **deletes its image** once a second `Ctrl-s` confirms — see [Additional Disks](../README.md#additional-disks) |
| Boot ISO | Path to an existing `.iso`, or blank to boot from disk (e.g. after installation). A running VM gets the new disc right away |
| Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot`; VM must be stopped. Turning Secure Boot on rebuilds the VM's UEFI NVRAM |
| TPM 2.0 | `disabled` · `enabled`; needs `swtpm` on the host |
| Network | `user (NAT)` · `tap (bridge)` · `none` |
| MAC Address | Leave blank to generate a new random one |
| Port Forwards | `user` mode only. Comma-separated `[tcp\|udp:]host:guest`, e.g. `2222:22, udp:5353:53` |
| VNC Display | `0` to disable; `1`–`99` |

Changes to a running VM are saved but only take effect the next time it is started, except the boot ISO and newly added disks, which are applied in the guest right away.

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field |
| `Shift-Tab` / `↑` | Previous field |
| `h` / `l` / `←` / `→` | Cycle a selector (firmware, TPM, network) |
| `Ctrl-s` (or `Enter` on **Save**) | Save changes |
| `Esc` | Cancel and return to VM detail |

## USB passthrough screen

Press `u` on the list or detail screen. It lists the USB devices connected to the host (hubs are left out — they stay with the host kernel) and marks the ones passed through to this VM. Devices that are configured but currently unplugged are listed at the bottom as *not connected*.

```
  Ostrich — USB Passthrough: debian-12

  ● running — attaching or detaching hot-plugs the device in the guest

  Host USB devices   [x] = passed through to this VM

  ▸ [x] 046d:085c  C922 Pro Stream Webcam                port 3-2.2.2
    [ ] 2972:0077  FiiO K11                              port 3-2.2.1
    [ ] 046d:c52b  Logitech USB Receiver                 port 3-2.2.3   ✗ no access
    [x] 0781:5583  SanDisk Ultra Fit                     not connected

  Space/Enter: attach/detach   a: add by ID   r: rescan   j/k: move   q/Esc: back
```

| Key | Action |
|-----|--------|
| `Space` / `Enter` | Attach or detach the device under the cursor |
| `a` | Add a device by `vendor:product` ID (as printed by `lsusb`), e.g. one that is not plugged in yet |
| `y` | Copy the udev command that grants access to the device under the cursor (shown when it is marked **✗ no access**) |
| `r` | Rescan host devices |
| `j` / `k`, `g` / `G` | Move cursor |
| `q` / `h` / `Esc` | Back to VM detail |

Every toggle is written to `vm.yaml` immediately. If the VM is running the device is hot-plugged (or unplugged) through the QEMU monitor on the spot; otherwise it is attached the next time the VM starts. See [USB Passthrough](../README.md#usb-passthrough) for the host-side permissions this needs.

## ISO hot-plug screen

Press `i` on the list or detail screen. The top shows the boot ISO in the VM's CD-ROM drive, which you can swap or eject; below it are the disk images attached to the VM as USB drives, which you can add to or detach from. On a running VM every change is applied in the guest on the spot, through the QEMU monitor; on a stopped VM it takes effect at the next start. Either way it is written to `vm.yaml` immediately.

```
  Ostrich — ISO Hot-plug: debian-12

  ● running — changes are applied in the guest right away

  Boot ISO (CD-ROM drive)

    /home/user/iso/debian-12.3.0-amd64-netinst.iso                            ● 631 MiB

  USB drives   images attached read-only; the guest sees each one as a USB stick

  ▸ /home/user/iso/virtio-win.iso                                              ● 611 MiB
    /home/user/iso/old-drivers.iso                                             ✗ not found

  c: change boot ISO   e: eject   a: attach USB image   Space/Enter/d: detach   r: refresh   j/k: move   q/Esc: back
```

| Key | Action |
|-----|--------|
| `c` | Put a different ISO in the CD-ROM drive: type its path (`~` is your home directory) and press `Enter` |
| `e` | Eject the boot ISO, leaving the drive empty |
| `a` | Attach an image as a USB drive, by path |
| `Space` / `Enter` / `d` | Detach the USB image under the cursor |
| `r` | Re-check the image files |
| `j` / `k`, `g` / `G` | Move cursor |
| `q` / `h` / `Esc` | Back to VM detail |

**Boot ISO.** The CD-ROM drive is always part of the VM, empty when no ISO is configured, so a disc can go in at any time. It sits where QEMU's `-cdrom` puts it, so guests see the same hardware as before. Swapping forces the tray open first (`eject -f cdrom`, then `change cdrom <path> raw`), so a guest that has locked it, as Linux does while the disc is mounted, cannot hold up the swap; it sees the disc change the way it would with a real drive. The edit form's Boot ISO field does the same when the VM is running. A VM whose boot ISO has gone missing refuses to start and names the file.

**USB drives.** Images are attached read-only, so the file is never modified and several VMs can share one; the guest sees a removable USB stick holding the image byte for byte. Each image is a drive plus a `usb-storage` device on the VM's xHCI controller. A VM whose image file has gone missing refuses to start, too.

```
-drive if=ide,index=2,id=cdrom,media=cdrom,format=raw,file=/home/user/iso/debian-12.3.0-amd64-netinst.iso
-drive if=none,id=usbimg-virtio-win.iso-drive,format=raw,readonly=on,file=/home/user/iso/virtio-win.iso
-device usb-storage,id=usbimg-virtio-win.iso,bus=xhci.0,drive=usbimg-virtio-win.iso-drive,removable=on
```

What the guest makes of a USB image depends on the image. A Linux guest mounts the ISO9660 filesystem straight off the stick (`mount /dev/sdb /mnt`), and a fresh VM with an empty disk boots a hybrid ISO — most Linux installers — from it under both BIOS and UEFI. Windows does not mount ISO9660 from a disk-class device, so a plain ISO shows up there as an unformatted drive; for Windows, put the ISO in the CD-ROM drive instead. Any raw disk image works as a USB drive, not just `.iso` files.

## Templates

A template is a stopped VM frozen as a starting point for new VMs: a copy of its disk image together with the machine definition the installed OS depends on (architecture, firmware, Secure Boot, TPM), its UEFI NVRAM (the boot entries) and its TPM state. Install and configure an OS once, save it as a template, and every VM made from it boots straight into that system with its own name, CPU, RAM, network and MAC address.

Templates are full copies: deleting the source VM, or the template, does not affect VMs made from it. They live in `.templates/` under the VM storage directory — see [VM Storage Layout](../README.md#vm-storage-layout).

### Save as template

Press `t` on the list or detail screen. **The VM must be stopped** — shut it down from inside the guest first. Ostrich refuses a running VM rather than stopping it, because stopping QEMU kills the machine without a guest shutdown and would leave the filesystem in the copy dirty, which is the opposite of what a template is for.

```
  Ostrich — Save as Template: debian-12

  ● stopped — the disk is in a consistent state and can be copied

  What goes into the template
  ╭──────────────────────────────────────────────────────────────────────────────────────╮
  │   Disk:     20 GiB virtual, 4.3 GiB on the host — copied in full                     │
  │   Firmware: UEFI, TPM 2.0 — with the UEFI NVRAM (boot entries) and the TPM state     │
  │   Defaults: 2 cores, 2048 MiB RAM, user network — chosen anew for each VM made from it│
  ╰──────────────────────────────────────────────────────────────────────────────────────╯
  Left out, as they belong to one VM: MAC address, port forwards, VNC display, boot ISO, USB devices and images, additional disks.

  ▸ Template name    debian-12-base
    Description      Debian 12 with docker and my dotfiles

      Save template

  Tab/↓: next   Shift+Tab/↑: back   Ctrl-s: save   Esc: cancel
```

| Field | Notes |
|-------|-------|
| Template name | Letters, digits, hyphens and underscores; defaults to the VM's name |
| Description | Optional, shown in the templates list |

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field |
| `Shift-Tab` / `↑` | Previous field |
| `Ctrl-s` (or `Enter` on **Save template**) | Save |
| `Esc` | Cancel and return to VM detail |

The disk image is copied with `qemu-img convert`, which writes a fresh, compact qcow2 holding only the allocated clusters; the virtual size stays the same. This takes a while for a large disk — the screen shows a spinner meanwhile and the TUI stays responsive. Nothing half-made is left behind if the copy fails.

What a template carries, and what it does not:

| Carried | Left out (belongs to one VM or to the host) |
|---------|---------------------------------------------|
| Disk image | MAC address (a new one is generated) |
| UEFI NVRAM — boot entries, Secure Boot keys | Port forwards (two VMs cannot share a host port) |
| TPM state — so BitLocker and Windows Hello still work | VNC display number (two VMs cannot share a port; a new VM gets a free one if the source had VNC) |
| Architecture, firmware, Secure Boot, TPM | Boot ISO (a clone would run the installer again) |
| CPU, RAM and network type, as defaults | USB devices and USB images |
| | Additional disks (they hold one VM's data, not the installed system) |

### Templates screen

Press `T` on the list screen. It lists the templates with the machine each one defines; the box below describes the one under the cursor.

```
  Ostrich — VM Templates

    debian-12-base        CPU: 2  RAM: 2048 MiB  Disk: 20 GiB   UEFI, TPM 2.0                user
  ▸ win11-base            CPU: 4  RAM: 8192 MiB  Disk: 64 GiB   UEFI + Secure Boot, TPM 2.0  user

  ╭──────────────────────────────────────────────────────────────────────────────────────╮
  │   Template: win11-base                                                               │
  │   About:    Windows 11 23H2, updates applied, virtio drivers installed               │
  │   From VM:  windows-11, saved 2026-10-09 13:33                                       │
  │   Disk:     64 GiB virtual, 18.2 GiB on the host                                     │
  │   Firmware: UEFI + Secure Boot, TPM 2.0                                              │
  │   Defaults: 4 cores, 8192 MiB RAM, user network — chosen anew for each VM            │
  │   VNC:      enabled — a new VM gets a free display number                            │
  ╰──────────────────────────────────────────────────────────────────────────────────────╯

  j/k: navigate  g/G: top/bottom  l/enter/n: new VM from template  d: delete  r: refresh  q/h/Esc: back
```

| Key | Action |
|-----|--------|
| `l` / `Enter` / `n` | Create a new VM from the template under the cursor |
| `d` | Delete the template (asks confirmation; VMs made from it are not affected) |
| `r` | Refresh |
| `j` / `k`, `g` / `G` | Move cursor |
| `q` / `h` / `Esc` | Back to VM list |

### New VM from template

A 6-step wizard, pre-filled with the template's defaults. The disk, architecture, firmware, Secure Boot and TPM come from the template and are not asked for.

| Step | Field | Notes |
|------|-------|-------|
| 1 | VM Name | Defaults to `<template>-1`, `-2`, … whichever is free |
| 2 | CPU Cores | Positive integer |
| 3 | RAM (MiB) | Minimum 64 |
| 4 | Network type | `user (NAT)` · `tap (bridge)` · `none` |
| 5 | Port forwards | `user` mode only; blank for none. Pick host ports no other VM uses |
| 6 | Confirm | Review and create |

```
  Ostrich — New VM from Template: win11-base

  Step 6 / 6   —   64 GiB disk, UEFI + Secure Boot, TPM 2.0: from the template

  Confirm
  Press Enter to create the VM. The disk is copied from the template, which takes a while for a large disk

  ╭─────────────────────────────────────────────────────────╮
  │   Name:     win11-test                                  │
  │   Template: win11-base                                  │
  │   CPU:      2 cores                                     │
  │   RAM:      4096 MiB                                    │
  │   Disk:     64 GiB (copied from the template)           │
  │   Firmware: UEFI + Secure Boot, TPM 2.0                 │
  │   Net:      user [tcp:3390:3389]                        │
  │   VNC:      display 2 (port 5902) — the lowest one free │
  ╰─────────────────────────────────────────────────────────╯

  Enter/j: create VM   k/Shift+Tab: back   Esc: cancel
```

The keys are those of the [create form](#create-vm-form). The new VM gets a fresh MAC address and, when the template's source VM had a VNC display, the lowest display number no existing VM uses; change either later in the edit form. Everything else about the VM — ISO, USB, port forwards, VNC — is edited the same way as for any other VM.

