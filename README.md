# Ostrich

A terminal UI for managing QEMU virtual machines, built with [Bubbletea](https://github.com/charmbracelet/bubbletea).

```
┌─ Ostrich — Virtual Machines ──────────────────────────────────────────────┐
│                                                                             │
│  debian-12             ● running  (PID 94231)   CPU: 2  RAM: 2048  Disk: 20 GiB │
│  ubuntu-24             ● stopped                CPU: 4  RAM: 4096  Disk: 40 GiB │
│  windows-11            ● stopped                CPU: 8  RAM: 8192  Disk: 80 GiB │
│                                                                             │
│  j/k: navigate  g/G: top/bottom  ^d/^u: half-page  l/enter: open          │
│  n: new  s: start  x: stop  d: delete  r: refresh  q: quit                │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Features

- **Create VMs** — guided multi-step form for CPU, RAM, disk, networking, and VNC
- **Start / stop VMs** — QEMU processes are detached and survive TUI exit
- **Live console view** — serial console output streamed in the detail screen, auto-refreshed every 2 seconds
- **Interactive serial console** — press `c` in the detail screen to connect directly to the VM's serial port (via `socat`); Ctrl-`]` to disconnect
- **VNC display** — optional VNC server per VM; press `v` to launch a VNC viewer (GUI)
- **USB passthrough** — press `u` to pick host USB devices for a VM; hot-plugged into a running VM, attached at boot otherwise
- **ISO hot-plug** — press `i` to swap or eject the boot ISO in a running VM's CD-ROM drive, and to attach ISOs (or any raw disk image) as read-only USB drives; applied in the running guest on the spot, at boot otherwise
- **UEFI, Secure Boot and TPM 2.0** — per-VM OVMF firmware with Microsoft's keys enrolled and an emulated TPM, which is what Windows 11 setup insists on — see [UEFI, Secure Boot and TPM 2.0](#uefi-secure-boot-and-tpm-20)
- **KVM auto-detection** — `-enable-kvm -cpu host` added automatically when `/dev/kvm` is accessible
- **Networking modes** — user/NAT (with optional port forwards), tap/bridge, or none
- **YAML config per VM** — human-readable, hand-editable `vm.yaml` in each VM folder
- **Vim keybindings** throughout

## Requirements

| Dependency | Purpose |
|---|---|
| `qemu-system-*` | Running virtual machines |
| `qemu-img` | Creating qcow2 disk images |
| Go 1.21+ | Building from source |
| OVMF / edk2 (optional) | UEFI firmware for `firmware: uefi` VMs |
| `swtpm` (optional) | Emulated TPM 2.0 for `tpm: true` VMs |
| `virt-fw-vars` (optional) | Enrolls Secure Boot keys where the OVMF package ships none (Arch, Homebrew); from the `virt-firmware` package |

Install QEMU on common distros:

```bash
# Debian / Ubuntu
sudo apt install qemu-system-x86 qemu-utils ovmf swtpm

# Fedora / RHEL
sudo dnf install qemu-system-x86 qemu-img edk2-ovmf swtpm

# Arch
sudo pacman -S qemu-full edk2-ovmf swtpm virt-firmware

# macOS (Homebrew) — QEMU bundles the edk2 images
brew install qemu swtpm && pip install virt-firmware
```

## Installation

### Pre-built binaries

Every tagged release ships static binaries for Linux and macOS (amd64 and arm64) on the [GitHub Releases](https://github.com/pzindyaev/ostrich/releases) page. Download the archive for your platform, verify it against `checksums.txt` if you like, and put the binary on your `PATH`:

```bash
# Example: Linux x86_64 — adjust the version and platform to taste
VERSION=1.0.0
curl -fsSLO https://github.com/pzindyaev/ostrich/releases/download/v${VERSION}/ostrich_${VERSION}_linux_amd64.tar.gz
tar -xzf ostrich_${VERSION}_linux_amd64.tar.gz ostrich
sudo install -m755 ostrich /usr/local/bin/ostrich
ostrich --version
```

### From source

```bash
git clone https://github.com/pzindyaev/ostrich
cd ostrich
make build        # or: go build -o ostrich .
./ostrich
```

Or install directly to `$GOPATH/bin`:

```bash
go install github.com/pzindyaev/ostrich@latest
```

`ostrich --version` prints the version, commit and build date baked in at build time (`dev` for plain `go build`).

## First Run

On first launch Ostrich shows a setup wizard asking for a **VM storage directory**. All VM sub-folders will be created here. The choice is saved to `~/.config/ostrich/config.json` and never asked again.

```
  Ostrich — QEMU Manager
  First-run setup

  VM Storage Directory
  Each VM will get its own sub-folder here containing its disk and config.

  > ~/VMs

  Enter — confirm   Ctrl+C — quit
```

The directory is created automatically if it does not exist.

## Usage

### VM List (main screen)

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
| `s` | Start selected VM |
| `x` | Stop selected VM |
| `d` | Delete selected VM (asks confirmation) |
| `r` | Refresh list and statuses |
| `q` | Quit |

### VM Detail screen

Shows configuration, running state and a scrollable view of the serial console output. The console refreshes automatically every 2 seconds.

| Key | Action |
|-----|--------|
| `s` | Start VM (when stopped) |
| `x` | Stop VM (when running) |
| `e` | Edit VM properties |
| `u` | **USB passthrough** — pick host devices for this VM |
| `i` | **ISO hot-plug** — swap the boot ISO, attach disk images as USB drives |
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

### Create VM form

A linear 10-step wizard. Text input fields accept free typing; the selectors (firmware, TPM, network) use `h/l` or arrow keys.

| Step | Field | Notes |
|------|-------|-------|
| 1 | VM Name | Letters, digits, hyphens, underscores |
| 2 | CPU Cores | Positive integer, e.g. `2` |
| 3 | RAM (MiB) | Minimum 64, e.g. `2048` for 2 GiB |
| 4 | Disk Size (GiB) | Minimum 1, e.g. `20` |
| 5 | Boot ISO | Full path to an `.iso` file, or leave blank |
| 6 | Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot` — see [UEFI, Secure Boot and TPM 2.0](#uefi-secure-boot-and-tpm-20) |
| 7 | TPM 2.0 | `disabled` · `enabled` (needs `swtpm`) |
| 8 | Network type | `user (NAT)` · `tap (bridge)` · `none` |
| 9 | VNC Display Number | `0` to disable; `1`–`99` enables VNC on TCP port `5900+N` |
| 10 | Confirm | Review and submit |

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `j` / `↓` | Next field |
| `Shift-Tab` / `k` / `↑` | Previous field |
| `h` / `l` / `←` / `→` | Cycle a selector (steps 6–8) |
| `Esc` | Cancel and return to VM list |

### Edit VM form

Press `e` on the list or detail screen to edit an existing VM. All properties are shown on one page, pre-filled with the current values.

| Field | Notes |
|-------|-------|
| Name | Renames the VM directory; VM must be stopped |
| CPU Cores / RAM (MiB) | Same rules as the create form |
| Disk Size (GiB) | Can only grow (`qemu-img resize`); VM must be stopped. The guest still has to extend its own partitions/filesystem |
| Boot ISO | Path to an existing `.iso`, or blank to boot from disk (e.g. after installation). A running VM gets the new disc right away |
| Firmware | `BIOS` · `UEFI` · `UEFI + Secure Boot`; VM must be stopped. Turning Secure Boot on rebuilds the VM's UEFI NVRAM |
| TPM 2.0 | `disabled` · `enabled`; needs `swtpm` on the host |
| Network | `user (NAT)` · `tap (bridge)` · `none` |
| MAC Address | Leave blank to generate a new random one |
| Port Forwards | `user` mode only. Comma-separated `[tcp\|udp:]host:guest`, e.g. `2222:22, udp:5353:53` |
| VNC Display | `0` to disable; `1`–`99` |

Changes to a running VM are saved but only take effect the next time it is started.

| Key | Action |
|-----|--------|
| `Tab` / `Enter` / `↓` | Next field |
| `Shift-Tab` / `↑` | Previous field |
| `h` / `l` / `←` / `→` | Cycle a selector (firmware, TPM, network) |
| `Ctrl-s` (or `Enter` on **Save**) | Save changes |
| `Esc` | Cancel and return to VM detail |

### USB passthrough screen

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

Every toggle is written to `vm.yaml` immediately. If the VM is running the device is hot-plugged (or unplugged) through the QEMU monitor on the spot; otherwise it is attached the next time the VM starts. See [USB Passthrough](#usb-passthrough) for the host-side permissions this needs.

### ISO hot-plug screen

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

## VM Storage Layout

```
~/VMs/                          ← configured on first run
├── debian-12/
│   ├── vm.yaml                 ← VM configuration (human-editable)
│   ├── disk.qcow2              ← QEMU qcow2 disk image
│   ├── efivars.fd              ← UEFI NVRAM: boot entries, Secure Boot keys (UEFI VMs only)
│   ├── tpm/                    ← emulated TPM state (TPM VMs only)
│   ├── swtpm.sock, swtpm.pid, swtpm.log   ← TPM emulator (while running)
│   ├── console.log             ← serial console log (written while running)
│   ├── serial.sock             ← serial console Unix socket (interactive, while running)
│   ├── qemu.pid                ← PID of the running QEMU process
│   └── qemu-monitor.sock       ← QEMU monitor Unix socket
└── ubuntu-24/
    ├── vm.yaml
    ├── disk.qcow2
    ├── console.log
    ├── serial.sock
    ├── qemu.pid
    └── qemu-monitor.sock
```

## VM Configuration File

Each VM is described by a `vm.yaml` in its directory. The file is written by the create wizard but can be edited by hand — changes take effect the next time the VM is started.

```yaml
name: debian-12
cpu: 2
ram: 2048        # MiB
disk_size: 20    # GiB (informational; actual size lives in disk.qcow2)
arch: x86_64
cdrom_path: /home/user/iso/debian-12.iso   # the CD-ROM drive; omit (or eject with i, e) after install
firmware: uefi      # bios (default) | uefi — see UEFI, Secure Boot and TPM 2.0
secure_boot: true   # enforce Secure Boot with Microsoft's keys; implies uefi
tpm: true           # emulated TPM 2.0 (swtpm)
network:
  type: user     # user | tap | none
  mac: 52:54:00:ab:cd:ef
  port_forwards:
    - host: 2222
      guest: 22
      proto: tcp
    - host: 8080
      guest: 80
      proto: tcp
vnc_port: 1      # VNC display 1 → TCP 5901; omit or set 0 to disable
usb_devices:     # host USB devices passed through — see USB Passthrough
  - vendor_id: "046d"
    product_id: "085c"
    name: C922 Pro Stream Webcam   # informational
  - vendor_id: "0781"
    product_id: "5583"
    port: 3-2.2.4                  # optional: pin to one physical port
usb_images:      # disk images attached as read-only USB drives — see ISO hot-plug screen
  - path: /home/user/iso/virtio-win.iso
created_at: 2026-03-17T09:00:00Z
```

### Network modes

| Mode | Description | Requirements |
|------|-------------|--------------|
| `user` | SLIRP/NAT — works out of the box, no host privileges required | none |
| `tap` | Bridged tap — full network access, uses host bridge `br0` | `br0` must exist and be allowed in `/etc/qemu/bridge.conf` — see [Setting up `br0`](#setting-up-br0) |
| `none` | No network interface attached | — |

Port forwards (`port_forwards`) are only used in `user` mode. They map a host TCP/UDP port to a guest port, e.g. `host: 2222 → guest: 22` lets you `ssh -p 2222 localhost` to reach the VM's SSH daemon.

In `user` mode each VM sits on its own private SLIRP network (`10.0.2.0/24`), so the guest's DHCP address is always `10.0.2.15` (gateway `10.0.2.2`). The detail view shows it while the VM is running. The address is internal to QEMU and not reachable from the host — use port forwards to get in.

In `tap` mode the address comes from whatever DHCP server serves the bridge. The detail view finds it by the VM's MAC, checking local dnsmasq lease files first and then the host's ARP table (`/proc/net/arp`, Linux only). With the NetworkManager setup below the lease file is root-only, so the ARP table is what gets used; the entry appears as soon as the guest has completed DHCP.

### Setting up `br0`

`tap` mode starts QEMU with `-netdev bridge,id=net0,br=br0`. The tap device is created by the setuid `qemu-bridge-helper`, so ostrich itself needs no root — but the bridge has to exist and the helper has to be told it may use it.

**1. Allow the bridge for the helper** (all setups):

```sh
echo 'allow br0' | sudo tee -a /etc/qemu/bridge.conf
ls -l /usr/lib/qemu/qemu-bridge-helper    # must be setuid root (-rwsr-xr-x)
```

**2. Create the bridge.** Pick one:

*a) NAT'd bridge via NetworkManager* — works on any uplink, including Wi-Fi (which cannot be enslaved to a bridge) and laptops that roam between networks. NetworkManager runs a private dnsmasq (DHCP + DNS) on the bridge, masquerades traffic out of whatever the current uplink is, and enables forwarding on the interfaces involved. VMs can reach each other, the host and the internet; the host can reach the VMs; the LAN cannot.

```sh
sudo nmcli con add type bridge ifname br0 con-name br0 \
    ipv4.method shared ipv4.addresses 192.168.76.1/24 \
    ipv6.method disabled bridge.stp no connection.autoconnect yes
sudo nmcli con up br0
```

Choose a subnet that doesn't collide with your LAN or VPN routes. Guests get `192.168.76.10–254`. The connection is persistent across reboots. `br0` shows `NO-CARRIER` until the first VM attaches — that's normal.

*b) True L2 bridge onto a wired NIC* — VMs appear on your LAN and get addresses from your router. Ethernet only:

```sh
sudo nmcli con add type bridge ifname br0 con-name br0 bridge.stp no
sudo nmcli con add type bridge-slave ifname enp3s0 master br0
sudo nmcli con up br0      # the host's IP moves from enp3s0 to br0
```

**3. Firewall.** With a default-deny firewall the guests' DHCP/DNS requests to the host and their routed traffic must be allowed. For ufw and setup (a):

```sh
sudo ufw allow in on br0 to any port 67 proto udp   # DHCP
sudo ufw allow in on br0 to any port 53             # DNS
sudo ufw route allow in on br0                      # guest → internet
```

**4. Verify** without any guest image — a diskless VM will PXE-boot and request a lease:

```sh
qemu-system-x86_64 -display none -boot n \
    -netdev bridge,id=net0,br=br0 \
    -device virtio-net-pci,netdev=net0,mac=52:54:00:00:00:01 &
sleep 20; grep -i 52:54:00:00:00:01 /proc/net/arp; kill %1
```

To undo: `sudo nmcli con delete br0`, remove the `allow br0` line, and `sudo ufw status numbered` / `sudo ufw delete <n>` for the rules.

## UEFI, Secure Boot and TPM 2.0

A VM boots SeaBIOS unless `firmware: uefi` is set. A UEFI VM gets the host's OVMF (edk2) firmware on two flash devices: the read-only code image, shared by every VM, and `efivars.fd`, the VM's private NVRAM holding its boot entries and Secure Boot keys. The NVRAM is copied from the firmware's template when the VM is created (or on first start, for a hand-edited `vm.yaml`).

| Setting | Effect |
|---|---|
| `firmware: uefi` | OVMF on pflash instead of SeaBIOS. The firmware boots the disk once an OS is on it and tries the ISO before that |
| `secure_boot: true` | The Secure Boot build of OVMF (`-machine q35,smm=on`), with a generated platform key and Microsoft's KEK and db certificates (2011 and 2023 generations) enrolled in the NVRAM. Windows and shim-signed Linux boot; unsigned loaders are refused with *Access Denied*. Implies `firmware: uefi` |
| `tpm: true` | An emulated TPM 2.0: `swtpm` is started next to QEMU with its state in `tpm/`, and the guest sees a `tpm-tis` device. The state persists across restarts, so BitLocker and the like keep working |

QEMU is started with:

```
-machine q35,smm=on
-global driver=cfi.pflash01,property=secure,value=on
-drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd
-drive if=pflash,format=raw,unit=1,file=~/VMs/windows-11/efivars.fd
-chardev socket,id=chrtpm,path=~/VMs/windows-11/swtpm.sock
-tpmdev emulator,id=tpm0,chardev=chrtpm
-device tpm-tis,tpmdev=tpm0
```

and `swtpm` as `swtpm socket --tpm2 --tpmstate dir=~/VMs/windows-11/tpm --ctrl type=unixio,path=~/VMs/windows-11/swtpm.sock --terminate --daemon`. It exits by itself when QEMU disconnects.

### Finding the firmware

Ostrich reads QEMU's firmware descriptors the way libvirt does — `/usr/share/qemu/firmware/*.json`, overridable per file from `/etc/qemu/firmware` and `~/.config/qemu/firmware` — and picks the first UEFI image for the VM's architecture and machine type. For plain UEFI it prefers an image without Secure Boot (no SMM needed); for Secure Boot it needs one with the `secure-boot` feature and prefers a template whose keys are already enrolled. Hosts without descriptors fall back to the usual package paths (Fedora, Debian/Ubuntu, Arch, the images bundled with QEMU and Homebrew). If nothing is found, creating or starting the VM fails with the package to install.

### Secure Boot keys

Secure Boot only enforces anything once a platform key and the signing certificates are enrolled. Fedora and Debian/Ubuntu ship a vars template with Microsoft's keys already in it (`OVMF_VARS.secboot.fd`, `OVMF_VARS_4M.ms.fd`), which is copied as is. Arch's `edk2-ovmf` and the images bundled with QEMU do not, so Ostrich runs `virt-fw-vars` from the [virt-firmware](https://gitlab.com/kraxel/virt-firmware) package when the VM is created:

```sh
virt-fw-vars --input /usr/share/edk2/x64/OVMF_VARS.4m.fd --output efivars.fd \
    --enroll-generate ostrich --microsoft-kek all --microsoft-db all --secure-boot
```

The platform key is generated for the VM and thrown away; the db gets *Microsoft Windows Production PCA 2011*, *Windows UEFI CA 2023*, *Microsoft Corporation UEFI CA 2011*, *Microsoft UEFI CA 2023* and the option ROM CA. Without `virt-fw-vars` the VM is not created — pick `UEFI` instead, or install the package. Turning Secure Boot on for an existing VM rebuilds `efivars.fd` with the keys; the firmware re-creates the boot entries (Windows and Linux both leave a fallback loader at `\EFI\BOOT\BOOTX64.EFI`). Turning it off keeps the file.

### Installing Windows 11

1. Create the VM with at least 2 cores, 4096 MiB and 64 GiB, the Windows ISO as boot ISO, firmware `UEFI + Secure Boot`, TPM `enabled` and a VNC display. Windows 10 is happy with plain `BIOS`.
2. Start it and press `v`. The ISO asks to *Press any key to boot from CD or DVD* — do so within a few seconds, or OVMF drops into its boot menu (pick the DVD-ROM there, or stop and start the VM).
3. Windows setup has no drivers for the virtio disk and network card, so its disk list is empty. Get the [virtio-win driver ISO](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/latest-virtio/virtio-win.iso), copy its contents to a USB stick, pass the stick through with `u`, and choose *Load driver* → `viostor\w11\amd64` in setup. Install `NetKVM\w11\amd64` the same way from Device Manager after the first boot.
4. Once installed, eject the boot ISO: press `i` on the running VM, then `e` (or clear the Boot ISO in the edit form).

## Serial Console

Each VM's serial port is connected to a Unix socket (`serial.sock`) and simultaneously logged to `console.log`.

```
-chardev socket,id=serial0,path=<serial.sock>,server=on,wait=off,logfile=<console.log>
-serial chardev:serial0
```

This means:

- The **detail screen** always shows the last 200 lines of `console.log`, auto-refreshed every 2 seconds — no connection needed.
- Pressing **`c`** in the detail screen suspends the TUI and drops you into a live, bidirectional terminal session with the VM's serial console. Press **Ctrl-`]`** to disconnect and return to Ostrich.

The interactive connection uses `socat`:

```bash
# What Ostrich runs internally when you press c:
socat -,escape=0x1d UNIX-CONNECT:~/VMs/debian-12/serial.sock

# You can also connect from a second terminal at any time:
socat -,raw,echo=0 UNIX-CONNECT:~/VMs/debian-12/serial.sock
```

Install `socat` if not already present:

```bash
sudo apt install socat      # Debian/Ubuntu
sudo dnf install socat      # Fedora
brew install socat          # macOS
```

> **Note:** Only one client can connect to the socket at a time. If you have an active `socat` session from the TUI, a second terminal connection will block until you disconnect.

## VNC Display

Set `vnc_port` to a display number (1–99) to enable a VNC server for the VM. Ostrich passes `-vnc 127.0.0.1:<n>` to QEMU, binding the VNC server to localhost on TCP port `5900 + n`.

With VNC enabled, pressing **`v`** in the detail screen launches a VNC viewer in the background (the TUI stays open).

```
VNC display 1  →  TCP port 5901  →  connect with: vncviewer 127.0.0.1::5901
```

Ostrich tries these viewers in order: `vncviewer`, `tigervnc`, `xtightvncviewer`, `krdc`, `vinagre`, `remmina`.

Install TigerVNC (recommended):

```bash
sudo apt install tigervnc-viewer    # Debian/Ubuntu
sudo dnf install tigervnc           # Fedora
brew install --cask tigervnc-viewer # macOS
```

To connect manually from another machine (replace `<host>` and `<n>`):

```bash
# Full port form (unambiguous, works with all viewers):
vncviewer <host>::<port>
# e.g. for display 1:
vncviewer 127.0.0.1::5901
```

> **Security:** VNC is bound to `127.0.0.1` only. To expose it remotely, use an SSH tunnel: `ssh -L 5901:127.0.0.1:5901 user@host`, then `vncviewer 127.0.0.1:1`.

## USB Passthrough

A VM can take over USB devices plugged into the host — a webcam, a USB stick, a security key, a serial adapter — and the guest sees them as if they were plugged into it directly. While the guest holds a device the host cannot use it; it comes back when the device is detached or the VM stops.

Ostrich always gives a VM an xHCI controller and attaches each configured device to it:

```
-device qemu-xhci,id=xhci
-device usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c
```

Devices are matched by vendor/product ID, so they keep working when replugged into a different port or after a reboot. A device that is configured but not plugged in does not stop the VM from starting — QEMU picks it up as soon as it appears. When two identical devices are connected the picker pins the one you chose to its physical port (`hostbus`/`hostport`); you can also set `port` in `vm.yaml` by hand, using the sysfs name that `dmesg` prints (`usb 3-2.2.4: new high-speed USB device`).

Because the controller is always present, devices can be hot-plugged into a running VM: the picker sends `device_add` / `device_del` over the monitor socket. A VM started by an older Ostrich has no controller yet, so hot-plug fails until it is restarted — the change is saved and applied on the next start either way.

### Host permissions

QEMU runs as your user and opens the device node under `/dev/bus/usb/`, which is normally writable by root only. Without access QEMU starts fine but never attaches the device and only complains on its (discarded) stderr, so Ostrich checks first: the picker flags such devices with **✗ no access**, and starting a VM whose device is plugged in but inaccessible fails with the command needed to fix it.

Move the cursor onto a flagged device and the picker shows the command that grants access, ready to run in another terminal — select it with the mouse or press `y` to copy it to the clipboard (needs `wl-copy` or `xclip`), run it, then press `r` to rescan:

```sh
echo 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",' 'ATTR{idProduct}=="085c",' 'TAG+="uaccess"' \
  | sudo tee -a /etc/udev/rules.d/70-ostrich-usb.rules \
  && sudo udevadm control --reload && sudo udevadm trigger
```

It appends one udev rule for the device, matched by ID, to `/etc/udev/rules.d/70-ostrich-usb.rules` (the file must sort before `73-seat-late.rules` for the `uaccess` tag to work), reloads udev and re-applies the rules to connected devices:

```
SUBSYSTEM=="usb", ATTR{idVendor}=="046d", ATTR{idProduct}=="085c", TAG+="uaccess"
```

`uaccess` gives the user logged in at the local seat an ACL on the node (systemd-logind). On a headless host or for a non-seat user, edit the rule to use a group instead: `..., MODE="0660", GROUP="plugdev"` and add your user to that group.

### Notes

- Hubs cannot be passed through; pass through the devices behind them.
- A keyboard or mouse given to a VM is gone from the host until it is detached — keep another way to drive the TUI.
- Host enumeration reads Linux sysfs. On other systems the picker cannot list devices, but `a` still adds them by ID.
- `lsusb` shows the same `vendor:product` IDs the picker uses.

## QEMU Process Model

- QEMU is launched with `setsid`, placing it in its own process group. **VMs keep running after Ostrich exits.**
- The PID is written to `qemu.pid`; liveness is checked with `kill -0` each time the list or detail screen refreshes.
- Stop sends `SIGTERM` and waits up to 5 seconds; if the process is still alive it sends `SIGKILL`.
- The serial console (`-serial file:console.log`) captures all text output from the VM (GRUB, kernel messages, login prompt, shell). This is what the detail screen displays.
- The QEMU monitor socket (`qemu-monitor.sock`) is available for direct interaction via `socat` or `nc`:

  ```bash
  socat - UNIX-CONNECT:~/VMs/debian-12/qemu-monitor.sock
  ```

## App Configuration

Stored at `~/.config/ostrich/config.json`:

```json
{
  "vm_storage_path": "/home/user/VMs"
}
```

To reconfigure the storage path, edit this file or delete it to trigger the first-run wizard again.

## Project Structure

```
ostrich/
├── main.go
├── go.mod
├── Makefile
├── .goreleaser.yaml         # release build matrix, archives, changelog
├── .github/workflows/
│   └── release.yml          # builds a GitHub release on every v* tag
└── internal/
    ├── config/
    │   └── config.go        # app config load/save
    ├── vm/
    │   ├── vm.go            # VMConfig struct, YAML schema, path helpers
    │   ├── manager.go       # list / create / delete VMs, qemu-img wrapper
    │   ├── process.go       # start / stop / status, console log reader
    │   ├── cdrom.go         # the CD-ROM drive holding the boot ISO, hot-swap
    │   ├── usb.go           # host USB enumeration (sysfs), passthrough config, hot-plug
    │   ├── usbimage.go      # disk images attached as USB drives (ISO hot-plug)
    │   └── monitor.go       # HMP monitor socket client
    └── tui/
        ├── app.go           # root Bubbletea model, screen router
        ├── styles.go        # Lipgloss colour palette and styles
        ├── setup.go         # first-run wizard
        ├── list.go          # VM list screen
        ├── create.go        # VM creation form
        ├── edit.go          # VM edit form
        ├── usb.go           # USB passthrough picker
        ├── iso.go           # ISO hot-plug screen
        └── detail.go        # VM detail + live console view
```
