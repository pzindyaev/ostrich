package vm

import "fmt"

// cdromDriveID is the QEMU drive ID of the VM's CD-ROM, which holds the boot
// ISO. The drive is always there, empty when no ISO is configured, so a
// running VM can be given one.
const cdromDriveID = "cdrom"

// cdromArgs returns the QEMU arguments for the CD-ROM drive with path in it
// ("" for an empty drive). On x86 it sits where -cdrom would put it, IDE
// index 2 (the q35 machine's third SATA port); the virt machine has no IDE,
// so there it hangs off a virtio-scsi controller.
func cdromArgs(machine, path string) []string {
	opts := "id=" + cdromDriveID + ",media=cdrom"
	if path != "" {
		opts += ",format=raw,file=" + qemuOptEscape(path)
	}
	if machine == "virt" {
		return []string{
			"-drive", "if=none," + opts,
			"-device", "virtio-scsi-pci,id=scsi0",
			"-device", "scsi-cd,bus=scsi0.0,drive=" + cdromDriveID,
		}
	}
	return []string{"-drive", "if=ide,index=2," + opts}
}

// CDROMChange puts a different ISO into the running VM's CD-ROM drive, or
// takes the disc out when path is "". The tray is forced open first, so a
// guest that has locked it (Linux does while the disc is mounted) cannot hold
// up the swap; it sees the disc change the way it would with a real drive.
func CDROMChange(storagePath, name, path string) error {
	if path != "" {
		if err := checkImage(path); err != nil {
			return err
		}
	}
	if err := monitorMustSucceed(storagePath, name, "eject -f "+cdromDriveID); err != nil {
		return err
	}
	if path == "" {
		return nil
	}
	return monitorMustSucceed(storagePath, name, fmt.Sprintf("change %s %s raw", cdromDriveID, hmpQuote(path)))
}
