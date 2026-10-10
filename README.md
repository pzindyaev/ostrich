# Ostrich

A terminal dashboard for managing QEMU virtual machines, written in Rust with [ratatui](https://ratatui.rs).

```
╭ Virtual Machines (3) ──────────╮╭ debian-12 ─────────────────────────────────────────────────────╮
│ ▸ debian-12   ● running        ││ Status    ● running  (PID 767371)                              │
│   ubuntu-24   ● stopped        ││ CPU       2 cores                                              │
│   windows-11  ● stopped        ││ RAM       2048 MiB                                             │
│                                ││ Disk      20 GiB                                               │
│                                ││ ISO       (none)                                               │
│                                ││ Firmware  UEFI, TPM 2.0                                        │
│                                ││ Net       user [2222→22]                                       │
│                                ││ IP        10.0.2.15  —  ssh -p 2222 localhost                  │
│                                ││ VNC       127.0.0.1:1  (TCP port 5901)                         │
│                                ││ USB       (none)                                               │
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

The VMs and the templates are on the left; the right side follows the selection with the VM's details and its live serial console. Forms and device dialogs open in the right-hand column, so the list stays in view.

## Features

- **Create VMs** — guided multi-step form for CPU, RAM, disks, networking, and VNC
- **Start / stop VMs** — QEMU processes are detached and survive TUI exit
- **Live console view** — the selected VM's serial console output in its own pane, auto-refreshed every 2 seconds
- **Interactive serial console** — press `c` to connect directly to the selected VM's serial port (via `socat`); Ctrl-`]` to disconnect
- **VNC display** — optional VNC server per VM; press `v` to launch a VNC viewer (GUI)
- **USB passthrough** — press `u` to pick host USB devices for a VM; hot-plugged into a running VM, attached at boot otherwise
- **ISO hot-plug** — press `i` to swap or eject the boot ISO in a running VM's CD-ROM drive, and to attach ISOs (or any raw disk image) as read-only USB drives; applied in the running guest on the spot, at boot otherwise
- **ISO picker** — every ISO input offers the images used before, with their size or whether the file has gone, and takes the path of a new one
- **Multiple disks** — give a VM additional virtio disks when creating it or later in the edit form; a disk added to a running VM is hot-plugged on the spot — see [Additional Disks](#additional-disks)
- **VM templates** — press `t` to freeze a stopped VM (disk, UEFI NVRAM, TPM state) as a template; the templates pane lists them, and new VMs are made from one with their own name, CPU, RAM and network — see [Templates](docs/USAGE.md#templates)
- **UEFI, Secure Boot and TPM 2.0** — per-VM OVMF firmware with Microsoft's keys enrolled and an emulated TPM, which is what Windows 11 setup insists on — see [UEFI, Secure Boot and TPM 2.0](#uefi-secure-boot-and-tpm-20)
- **KVM auto-detection** — `-enable-kvm -cpu host` added automatically when `/dev/kvm` exists and the VM's architecture is the host's
- **Networking modes** — user/NAT (with optional port forwards), tap/bridge, or none
- **YAML config per VM** — human-readable, hand-editable `vm.yaml` in each VM folder
- **Vim keybindings** throughout

## Requirements

| Dependency | Purpose |
|---|---|
| `qemu-system-*` | Running virtual machines |
| `qemu-img` | Creating qcow2 disk images |
| Rust 1.88+ (`cargo`) | Building from source |
| OVMF / edk2 (optional) | UEFI firmware for `firmware: uefi` VMs |
| `swtpm` (optional) | Emulated TPM 2.0 for `tpm: true` VMs |
| `virt-fw-vars` (optional) | Enrolls Secure Boot keys where the OVMF package ships none (Arch, Homebrew); from the `virt-firmware` package |
| `socat` (optional) | The interactive serial console (`c`) — see [Serial Console](#serial-console) |
| A VNC viewer (optional) | `v` — see [VNC Display](#vnc-display) |

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

Every tagged release ships binaries for Linux (static, musl) and macOS, amd64 and arm64, on the [GitHub Releases](https://github.com/pzindyaev/ostrich/releases) page. Download the archive for your platform, verify it against `checksums.txt` if you like, and put the binary on your `PATH`:

```bash
# Example: Linux x86_64 — adjust the version and platform to taste
VERSION=1.0.0
curl -fsSLO https://github.com/pzindyaev/ostrich/releases/download/v${VERSION}/ostrich_${VERSION}_linux_amd64.tar.gz
tar -xzf ostrich_${VERSION}_linux_amd64.tar.gz
sudo install -m755 ostrich_${VERSION}_linux_amd64/ostrich /usr/local/bin/ostrich
ostrich --version
```

### From source

```bash
git clone https://github.com/pzindyaev/ostrich
cd ostrich
make build        # or: cargo build --release, which leaves it in target/release/
./ostrich
```

Or install straight into `~/.cargo/bin`:

```bash
cargo install --git https://github.com/pzindyaev/ostrich
```

`ostrich --version` prints `ostrich <version> (commit <commit>, built <date>)`, baked in at build time: for a plain `cargo build` the version comes from `git describe` and the date is that of the last commit, and cargo rebuilds the stamp after a commit, a new tag or a staged change (in a git worktree too); `OSTRICH_VERSION`, `OSTRICH_COMMIT` and `OSTRICH_DATE` in the environment override them, which is what `make` and the release workflow do.

## First Run

On first launch Ostrich shows a setup wizard asking for a **VM storage directory**. All VM sub-folders will be created here. The choice is saved to `~/.config/ostrich/config.json` and never asked again.

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

The directory is created automatically if it does not exist. `Esc` or `Ctrl-c` quits without saving anything, so the screen comes back on the next launch. It also comes up when `config.json` has no `vm_storage_path`, or a `null` or blank one — see [App Configuration](#app-configuration).

## Usage

The guide to the dashboard — the lists, the details and console panes, the create and edit forms, USB passthrough, ISO hot-plug and templates, with every key — is in [docs/USAGE.md](docs/USAGE.md).

Ostrich takes no options of its own: `ostrich` opens the dashboard, `ostrich --version` (or `version`, `-v`) prints the version line, and `ostrich --help` (or `help`, `-h`) a short usage. Any other argument, even one that is not valid UTF-8, is ignored and the dashboard opens. If `~/.config/ostrich/config.json` cannot be read or parsed, Ostrich prints `error initializing: <error>` and exits with status 1 before touching the terminal; with no terminal to draw on it prints `error: initialize terminal: <error>` and exits with status 1.

## VM Storage Layout

```
~/VMs/                          ← configured on first run
├── debian-12/
│   ├── vm.yaml                 ← VM configuration (human-editable)
│   ├── disk.qcow2              ← QEMU qcow2 disk image (the main disk)
│   ├── data.qcow2              ← an additional disk named "data" — see Additional Disks
│   ├── efivars.fd              ← UEFI NVRAM: boot entries, Secure Boot keys (UEFI VMs only)
│   ├── tpm/                    ← emulated TPM state (TPM VMs only)
│   ├── swtpm.sock, swtpm.pid, swtpm.log   ← TPM emulator (socket and PID while running)
│   ├── console.log             ← serial console log (written while running)
│   ├── qemu.log                ← QEMU's own output: warnings, and why it would not start
│   ├── serial.sock             ← serial console Unix socket (interactive, while running)
│   ├── qemu.pid                ← PID of the running QEMU process
│   └── qemu-monitor.sock       ← QEMU monitor Unix socket
├── ubuntu-24/
│   ├── vm.yaml
│   ├── disk.qcow2
│   ├── console.log
│   ├── qemu.log
│   ├── serial.sock
│   ├── qemu.pid
│   └── qemu-monitor.sock
└── .templates/                 ← VM templates — see Templates
    └── debian-12-base/
        ├── template.yaml       ← template description and machine definition
        ├── disk.qcow2          ← copy of the source VM's disk
        ├── efivars.fd          ← copy of its UEFI NVRAM (UEFI templates only)
        └── tpm/                ← copy of its TPM state (TPM templates only)
```

The templates directory starts with a dot so it can never clash with a VM: VM names may only contain letters, digits, hyphens and underscores.

## VM Configuration File

Each VM is described by a `vm.yaml` in its directory. The file is written by the create wizard but can be edited by hand — changes take effect the next time the VM is started.

```yaml
name: debian-12
cpu: 2
ram: 2048        # MiB
disk_size: 20    # GiB (informational; actual size lives in disk.qcow2)
disks:           # additional virtio disks — see Additional Disks
  - name: data   # <vm-dir>/data.qcow2; /dev/disk/by-id/virtio-data in the guest
    size: 50     # GiB
arch: x86_64
cdrom_path: /home/user/iso/debian-12.iso   # the CD-ROM drive; omit (or eject with i, e) after install
firmware: uefi      # bios (default) | uefi — see UEFI, Secure Boot and TPM 2.0
secure_boot: true   # enforce Secure Boot with Microsoft's keys; implies uefi
tpm: true           # emulated TPM 2.0 (swtpm)
network:         # leave it out for user (NAT) networking
  type: user     # user | tap | none; left out, empty or any other word is user
  mac: 52:54:00:ab:cd:ef   # or 52-54-00-ab-cd-ef, 5254.00ab.cdef; QEMU gets the colon form
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
usb_images:      # disk images attached as read-only USB drives — see the ISO hot-plug dialog
  - path: /home/user/iso/virtio-win.iso
created_at: 2026-03-17T09:00:00Z
```

A hand-edited file is read the way the Go version of Ostrich read it. Only `name` is required: a missing key, or one with an empty value (`null`, `~` or nothing after the colon), is the zero value — `0`, an empty string, `false`, an empty list. Booleans (`secure_boot`, `tpm`, and `vnc` in a template) also take the YAML 1.1 words `yes`/`no`, `on`/`off` and `y`/`n`, in lower case, capitalised or upper case. `created_at` may also be a date alone (`2026-10-09`) or a date and time without a zone (`2026-10-09 13:33:00`), both taken as UTC. Only `network.type: tap` and `network.type: none` mean something other than `user`: a file without `network:`, or with a missing, empty or unknown `network.type`, gets `user` (NAT) networking. A `user` or `tap` VM without a `mac` gets QEMU's default, `52:54:00:12:34:56`, so give each `tap` VM on the bridge its own. Values that are wrong do not hide the VM from the list; they are reported when they are used, starting the VM for example — `invalid USB device "046d":"" — vendor_id and product_id must be 4 hex digits`, `USB drive .: no image path`.

Ostrich writes `vm.yaml`, `template.yaml` and its own `config.json` atomically: the new contents go to a temporary file in the same directory, which is then renamed over the old file, so a crash or a full disk never leaves one cut short. The file keeps its permissions, a symlink stays a symlink (the file it points to is replaced, through a chain of links too, and created if it does not exist yet; a symlink loop is an error), and saving needs write permission on the directory that holds the file.

### Template file

Each template is described by a `template.yaml` in its directory under `.templates/`. It carries only what defines the machine; per-VM settings are chosen when a VM is made from it.

```yaml
name: debian-12-base
description: Debian 12 with docker and my dotfiles   # optional
source_vm: debian-12   # the VM it was made from (informational)
cpu: 2                 # defaults for a new VM, changeable on creation
ram: 2048
disk_size: 20          # GiB; the disk image is copied as is
arch: x86_64
firmware: uefi         # bios | uefi
secure_boot: false
tpm: true
network: user          # default for a new VM, changeable on creation
vnc: true              # the source had a VNC display; a new VM gets a free one
created_at: 2026-10-09T13:33:00Z
```

It is read like `vm.yaml`; a missing, empty or unknown `network` is `user`.

### Network modes

| Mode | Description | Requirements |
|------|-------------|--------------|
| `user` | SLIRP/NAT — works out of the box, no host privileges required | none |
| `tap` | Bridged tap — full network access, uses host bridge `br0` | `br0` must exist and be allowed in `/etc/qemu/bridge.conf`; the forms say what is missing and how to set it up — see [Setting up `br0`](#setting-up-br0) |
| `none` | No network interface attached (`-nic none`). The other devices keep the PCI addresses older versions gave them: the main disk `00:04.0`, the xHCI controller `00:03.0` and, on aarch64, the CD-ROM's SCSI controller `00:02.0`, so a UEFI guest installed under an older Ostrich still boots | — |

Port forwards (`port_forwards`) are only used in `user` mode. They map a host TCP/UDP port to a guest port, e.g. `host: 2222 → guest: 22` lets you `ssh -p 2222 localhost` to reach the VM's SSH daemon.

In `user` mode each VM sits on its own private SLIRP network (`10.0.2.0/24`), so the guest's DHCP address is always `10.0.2.15` (gateway `10.0.2.2`). The details pane shows it while the VM is running, followed by a ready `ssh -p <port> localhost` when a TCP port forward goes to guest port 22. The address is internal to QEMU and not reachable from the host — use port forwards to get in.

In `tap` mode the address comes from whatever DHCP server serves the bridge. The details pane finds it by the VM's MAC, checking local dnsmasq lease files first and then the host's ARP table (`/proc/net/arp`, Linux only). With the NetworkManager setup below the lease file is root-only, so the ARP table is what gets used; the entry appears as soon as the guest has completed DHCP.

### Setting up `br0`

`tap` mode starts QEMU with `-netdev bridge,id=net0,br=br0`. The tap device is created by the setuid `qemu-bridge-helper`, so ostrich itself needs no root — but the bridge has to exist and the helper has to be told it may use it.

Ostrich checks for both, and for the helper, whenever `tap` is picked in the create, edit or template form, and again when a `tap` VM is started. What is missing is shown with the commands that put it in place, ready to copy: the NetworkManager bridge from (a) below when `nmcli` is installed (plain `ip` commands otherwise), the `ufw` rules from step 3 when ufw is enabled, and the `allow` line from step 1.

```
⚠ tap networking is not set up on this host: the host has no bridge br0; /etc/qemu/bridge.conf does not allow it.
  Run this, then start the VM:

    sudo nmcli con add type bridge ifname br0 con-name br0 \
        ipv4.method shared ipv4.addresses 192.168.76.1/24 \
        ipv6.method disabled bridge.stp no connection.autoconnect yes
    sudo nmcli con up br0
    sudo ufw allow in on br0 to any port 67 proto udp
    sudo ufw allow in on br0 to any port 53
    sudo ufw route allow in on br0
    echo 'allow br0' | sudo tee -a /etc/qemu/bridge.conf

  This makes a NAT'd bridge that stays across reboots: guests get 192.168.76.10–254 by DHCP and reach the internet through the host. The ufw rules let the guests use the host's DHCP and DNS and route out.
  For a bridge onto a wired NIC, and to undo, see the README section "Setting up br0".
```

The rest of this section is the same setup by hand, with the alternatives.

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

## Additional Disks

Besides its main disk (`disk.qcow2`, `disk_size`), a VM can have up to 8 additional virtio disks: a data volume for Windows, a separate disk for a database, something to try LVM or RAID on. They are entered as a comma-separated list of `[name:]size` entries, sizes in GiB, in the create wizard's **Additional disks** step and the edit form's **Extra Disks** field. An entry without a name gets the lowest free `disk1`, `disk2`, …, so `data:50, 100` makes `data` (50 GiB) and `disk1` (100 GiB). Names are letters, digits, hyphens and underscores, start with a letter and are at most 20 characters; `disk` is taken by the main disk.

Each disk is a qcow2 image named after it in the VM folder (`data.qcow2`) and listed in `vm.yaml`:

```yaml
disks:
  - name: data
    size: 50
  - name: disk1
    size: 100
```

What the edit form does with a change to the list:

| Change | Stopped VM | Running VM |
|---|---|---|
| Add a disk | Image created, attached at the next start | Image created and hot-plugged into the guest on the spot |
| Grow a disk | `qemu-img resize`; the guest extends its own partitions | Refused — stop the VM first |
| Remove a disk | **Deletes the image.** The first Ctrl-s warns, naming the disks and sizes; the second confirms | Refused — stop the VM first |

Shrinking is refused, as for the main disk. Renaming a disk is a removal plus an addition: the old image is deleted (after the confirmation) and an empty one made under the new name.

In the guest each disk is a virtio-blk device whose serial is the disk's name, so it is `/dev/disk/by-id/virtio-data` whatever letter the kernel gives it (`/dev/vdb`, `/dev/vdc`, … in the order of the list; the main disk stays `/dev/vda`). Windows uses the same `viostor` driver as for the main disk.

A new disk arrives blank, without a partition table, so nothing shows it until it is partitioned and formatted in the guest. On Windows open Disk Management (`diskmgmt.msc`): the new disk is listed as *Not Initialized*; initialize it as GPT, then create a volume on the unallocated space and it gets a drive letter. On Linux partition it with `fdisk` or `parted` and make a filesystem with `mkfs`, or hand it to the installer's partitioner.

QEMU is started with:

```
-device pcie-root-port,id=disk-rp1,bus=pcie.0,chassis=1,addr=0x10
… through disk-rp8, chassis=8, addr=0x17
-drive if=none,id=disk-data-drive,format=qcow2,file=~/VMs/debian-12/data.qcow2
-device virtio-blk-pci,id=disk-data,drive=disk-data-drive,bus=disk-rp1,serial=data
```

Every VM gets the eight PCIe root ports whether it has extra disks or not, like the xHCI controller: the PCIe root bus does not take hot-plugged devices, a root port does, so a disk added while the VM runs goes onto the port its position in the list names (`drive_add`, then `device_add` over the monitor). The ports are pinned to high slots so the devices QEMU places by itself — the main disk, the xHCI controller, the network card — keep the PCI addresses they have always had (a VM without a network card has them pinned, see [Network modes](#network-modes)); OVMF boot entries record the disk's address, and existing UEFI VMs keep booting unchanged. A VM started by an older Ostrich has no ports yet, so hot-plug fails until it is restarted; the disk is created and saved either way and attached at the next start.

Notes:

- The boot order is untouched: SeaBIOS tries the main disk first and skips a disk without a boot sector; OVMF boots its NVRAM entries or a disk with an EFI system partition. A BIOS VM whose main disk is not bootable may fall through to a data disk.
- Templates carry the main disk only; make a VM from a template, then add disks in the edit form.
- Deleting a VM deletes all of its disks.

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

and `swtpm` as `swtpm socket --tpm2 --tpmstate dir=~/VMs/windows-11/tpm --ctrl type=unixio,path=~/VMs/windows-11/swtpm.sock --pid file=~/VMs/windows-11/swtpm.pid --log file=~/VMs/windows-11/swtpm.log --terminate --daemon`. It exits by itself when QEMU disconnects.

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
4. Once installed, eject the boot ISO: press `i` on the running VM, then `e` (or pick `(none)` for the Boot ISO in the edit form).

## Serial Console

Each VM's serial port is connected to a Unix socket (`serial.sock`) and simultaneously logged to `console.log`.

```
-chardev socket,id=serial0,path=<serial.sock>,server=on,wait=off,logfile=<console.log>
-serial chardev:serial0
```

This means:

- The **console pane** of the dashboard always shows the last 200 lines of `console.log` for the selected VM, auto-refreshed every 2 seconds — no connection needed. It follows the newest output until you scroll up. Only the end of the log is read, however large it grows; a line longer than 64 KiB shows only its last 64 KiB.
- Pressing **`c`** on a running VM suspends the TUI and drops you into a live, bidirectional terminal session with the VM's serial console, with the cursor shown. Your terminal is in raw mode for the session, so Ctrl-C, Ctrl-Z, Ctrl-`\`, Tab and the arrow keys go to the guest rather than to Ostrich. Press **Ctrl-`]`** to disconnect; the dashboard comes back with `✓ disconnected from serial console`. On a stopped VM `c` says `✗ VM is not running`, and without `socat` `✗ socat not found — install it…` with the commands below.

The interactive connection uses `socat`:

```bash
# What Ostrich runs internally when you press c:
socat -,raw,echo=0,escape=0x1d UNIX-CONNECT:~/VMs/debian-12/serial.sock

# You can also connect from a second terminal at any time (Ctrl-] disconnects):
socat -,raw,echo=0,escape=0x1d UNIX-CONNECT:~/VMs/debian-12/serial.sock
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

With VNC enabled, pressing **`v`** on a running VM launches a VNC viewer in the background (the TUI stays open) and says so: `✓ launched vncviewer → port 5901`. Otherwise it says why not: `✗ VNC is not enabled for this VM (vnc_port: 0)`, `✗ VM is not running`, or `✗ no VNC viewer found — …` with the commands below; a viewer that cannot be started gives `✗ launch VNC viewer: <viewer path>: <error>`.

```
VNC display 1  →  TCP port 5901  →  connect with: vncviewer 127.0.0.1::5901
```

Ostrich tries these viewers in order: `vncviewer`, `tigervnc`, `xtightvncviewer`, `remmina`, `krdc`, `vinagre`.

Install TigerVNC (recommended):

```bash
sudo apt install tigervnc-viewer    # Debian/Ubuntu
sudo dnf install tigervnc           # Fedora
brew install --cask tigervnc-viewer # macOS
```

To connect manually (replace `<host>` and `<port>`; from another machine only through the SSH tunnel below):

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

QEMU runs as your user and opens the device node under `/dev/bus/usb/`, which is normally writable by root only. Without access QEMU starts fine but never attaches the device and only complains in `qemu.log`, so Ostrich checks first: the picker flags such devices with **✗ no access**, and starting a VM whose device is plugged in but inaccessible fails with the command needed to fix it.

Move the cursor onto a flagged device and the picker shows the command that grants access, ready to run in another terminal — select it with the mouse or press `y` to copy it to the clipboard, run it, then press `r` to rescan. Copying uses `wl-copy` in a Wayland session (`WAYLAND_DISPLAY` set, and `wl-paste` installed too), otherwise `xclip`, then `xsel` (`termux-clipboard-set` under Termux, `clip.exe` under WSL), and `pbcopy` on macOS:

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

- QEMU is launched with `setsid`, placing it in its own process group. **VMs keep running after Ostrich exits** — however it exits: `q`, `Ctrl-c`, a `SIGINT` or `SIGTERM` (which quit the way `Ctrl-c` does, see [Quitting](docs/USAGE.md#quitting)), or a `SIGHUP` when the terminal is closed, which ends Ostrich at once.
- The PID is written to `qemu.pid`; liveness is checked with `kill -0`, and on Linux the process state in `/proc`, each time the dashboard refreshes, so a QEMU that has died counts as stopped even before it is reaped. On Linux the PID must also still belong to a QEMU (`/proc/<pid>/cmdline`): a `qemu.pid` left over from a crash or a reboot, whose PID some other program has since been given, counts as stopped and is removed, and that program is never signalled. `swtpm.pid` is checked the same way. Ostrich reaps the QEMU processes it started as they exit; one that outlives Ostrich is reaped by init.
- QEMU's own stdout and stderr go to `qemu.log` in the VM folder, rewritten on every start. A QEMU that refuses its command line, cannot open a disk or ISO, or is denied a bridge dies within moments: start waits for it to either exit or come up (its monitor answering), up to 3 seconds, and a start that fails reports the last 8 lines of that output on the spot (with the path of the log when there is more), for example:

  ```
  ✗ QEMU exited during startup (exit status 1):
      access denied by acl file
      qemu-system-x86_64: -netdev bridge,id=net0,br=br0: bridge helper failed
  ```

- Stop sends `SIGTERM` and waits up to 5 seconds; if the process is still alive it sends `SIGKILL`. The TPM emulator, if any, is stopped with it.
- The serial console (`-chardev socket,...,logfile=console.log`) captures all text output from the VM (GRUB, kernel messages, login prompt, shell). This is what the console pane displays.
- The QEMU monitor socket (`qemu-monitor.sock`) is available for direct interaction via `socat` or `nc`:

  ```bash
  socat - UNIX-CONNECT:~/VMs/debian-12/qemu-monitor.sock
  ```

  The monitor serves one client at a time, and Ostrich connects to it for every hot-plug (USB devices, the boot ISO, USB drives, new disks). While you hold it, those wait up to 5 seconds and then fail with `connect to QEMU monitor: <path>: i/o timeout`; disconnect and try again.

## App Configuration

Stored at `~/.config/ostrich/config.json`:

```json
{
  "vm_storage_path": "/home/user/VMs",
  "recent_isos": [
    "/home/user/iso/debian-12.3.0-amd64-netinst.iso",
    "/home/user/iso/virtio-win.iso"
  ]
}
```

To reconfigure the storage path, edit this file or delete it to trigger the first-run wizard again. A `vm_storage_path` that is missing, `null` or blank brings up the wizard too, instead of putting the VMs in whatever directory Ostrich was started from; the `recent_isos` already in the file are kept when it saves. Otherwise a missing key or a `null` is the empty value, as for `vm.yaml`; a file that is not valid JSON, or cannot be read, stops Ostrich at launch with `error initializing: <error>`. The file is saved atomically, like `vm.yaml` (see [VM Configuration File](#vm-configuration-file)), so it may be a symlink into a dotfiles repository.

`recent_isos` is the list the [ISO picker](docs/USAGE.md#choosing-an-iso) offers: every image put in a CD-ROM drive or attached as a USB drive is added at the top, the newest 20 are kept, and `d` in the picker takes one out. Images that a VM still has in its `vm.yaml` are offered whether or not they are in this list.

## Project Structure

```
ostrich/
├── Cargo.toml
├── build.rs                 # bakes version, commit and date into the binary
├── Makefile
├── docs/
│   └── USAGE.md             # the guide to the dashboard, pane by pane
├── scripts/
│   ├── package.sh           # release archive for one target (root-owned, commit-dated)
│   └── changelog.sh         # release notes from the git history
├── .github/workflows/
│   └── release.yml          # builds a GitHub release on every v* tag
└── src/
    ├── main.rs              # --version / --help, then the dashboard
    ├── lib.rs
    ├── config.rs            # app config (~/.config/ostrich/config.json), recent ISOs
    ├── vm/
    │   ├── config.rs        # VmConfig, the vm.yaml schema, path helpers
    │   ├── manager.rs       # list / create / update / delete VMs
    │   ├── process.rs       # QEMU command line, start / stop / status, console tail
    │   ├── monitor.rs       # HMP monitor socket client
    │   ├── cdrom.rs         # the CD-ROM drive holding the boot ISO, hot-swap
    │   ├── disk.rs          # additional disks: config, images, QEMU args, hot-plug
    │   ├── usb.rs           # host USB enumeration (sysfs), passthrough config, hot-plug
    │   ├── usbimage.rs      # disk images attached as USB drives (ISO hot-plug)
    │   ├── firmware.rs      # OVMF discovery, per-VM NVRAM, Secure Boot key enrolment
    │   ├── tpm.rs           # swtpm
    │   ├── bridge.rs        # what tap networking needs on the host, and how to set it up
    │   ├── guestip.rs       # the guest's IP: SLIRP, dnsmasq leases, ARP
    │   └── template.rs      # VM templates
    └── tui/
        ├── app.rs           # the dashboard: event loop, focus, keys, panels, popups
        ├── dashboard.rs     # the lists, the details card, the status and key bars
        ├── console.rs       # serial output sanitising and the console pane
        ├── actions.rs       # background work: load, start, stop, serial console, VNC, clipboard
        ├── panel.rs         # the contract between the dashboard and its forms and dialogs
        ├── events.rs        # task results and the event types
        ├── widgets.rs       # text input, selector, list cursor, spinner, text helpers
        ├── theme.rs         # the palette
        ├── popups.rs        # confirmations, long errors, the key help
        ├── setup.rs         # first-run wizard
        ├── forms/           # create, edit, new VM from template, save as template
        └── dialogs/         # USB passthrough, ISO hot-plug, the ISO picker
```
