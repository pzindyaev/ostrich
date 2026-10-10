//! Everything that touches QEMU, disk images, firmware and the host: the
//! `vm.yaml` schema, the VM manager, the QEMU process model, the HMP
//! monitor, devices (CD-ROM, extra disks, USB passthrough, USB images),
//! UEFI firmware and the TPM emulator, the host bridge, guest IP lookup and
//! VM templates.
//!
//! Every function here is synchronous and may block on the filesystem or on
//! external commands; the TUI calls them from background threads.

pub mod bridge;
pub mod cdrom;
pub mod config;
pub mod disk;
pub mod firmware;
pub mod guestip;
pub mod manager;
pub mod monitor;
pub mod process;
pub mod template;
pub mod tpm;
pub mod usb;
pub mod usbimage;

pub use bridge::{bridge_hint, check_bridge, BRIDGE_NAME};
pub use cdrom::cdrom_change;
pub use config::*;
pub use disk::{
    check_extra_disks, diff_disks, disk_hotplug, disk_names, disk_states, format_disks,
    parse_disks, validate_disks, Disk, DiskChange, MAX_EXTRA_DISKS,
};
pub use firmware::{arch_of, ensure_firmware_vars, find_firmware, machine_of, Firmware};
pub use guestip::guest_ip;
pub use manager::Manager;
pub use monitor::monitor_command;
pub use process::{
    build_qemu_args, read_console_tail, start, status, stop, ProcessInfo, VmStatus, USER_NET_CIDR,
    USER_NET_GATEWAY, USER_NET_GUEST_IP,
};
pub use template::{
    load_template, save_template, template_dir, template_disk_path, template_disk_usage,
    template_file_path, template_firmware_vars_path, template_tpm_dir, templates_dir, Template,
};
pub use tpm::check_tpm;
pub use usb::{
    check_usb_access, list_host_usb_devices, match_usb, parse_usb_id, udev_rule_command,
    udev_rule_command_words, udev_rule_hint, usb_device_ids, usb_hotplug, usb_hotunplug,
    usb_states, HostUsbDevice, UsbDevice, UsbState, UDEV_RULES_FILE,
};
pub use usbimage::{
    check_usb_images, image_state_of, usb_image_hotplug, usb_image_hotunplug, usb_image_ids,
    usb_image_states, ImageState, UsbImage,
};
