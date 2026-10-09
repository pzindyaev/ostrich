package vm

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"testing"
)

func TestParseFormatDisks(t *testing.T) {
	cases := []struct {
		in     string
		want   []Disk
		format string
	}{
		{"", nil, ""},
		{"data:50, 100", []Disk{{"data", 50}, {"disk1", 100}}, "data:50, disk1:100"},
		{" scratch : 10 ,", []Disk{{"scratch", 10}}, "scratch:10"},
		// Unnamed entries take the lowest free number, around the named ones.
		{"disk1:1, 5, disk3:2, 7", []Disk{{"disk1", 1}, {"disk2", 5}, {"disk3", 2}, {"disk4", 7}}, "disk1:1, disk2:5, disk3:2, disk4:7"},
		{"DISK1:1, 5", []Disk{{"DISK1", 1}, {"disk2", 5}}, "DISK1:1, disk2:5"},
	}
	for _, c := range cases {
		got, err := ParseDisks(c.in)
		if err != nil {
			t.Errorf("ParseDisks(%q): %v", c.in, err)
			continue
		}
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("ParseDisks(%q) = %+v, want %+v", c.in, got, c.want)
		}
		if f := FormatDisks(got); f != c.format {
			t.Errorf("FormatDisks(%+v) = %q, want %q", got, f, c.format)
		}
		if again, _ := ParseDisks(FormatDisks(got)); !reflect.DeepEqual(again, got) {
			t.Errorf("round trip of %q gave %+v", c.in, again)
		}
	}

	bad := map[string]string{
		"data":                                "expected [name:]size",
		"data:x":                              "expected [name:]size",
		":5":                                  "expected [name:]size",
		"data:0":                              "at least 1 GiB",
		"disk:5":                              "taken by the main disk",
		"Disk:5":                              "taken by the main disk",
		"1data:5":                             "starting with a letter",
		"a b:5":                               "letters, digits",
		"averyveryverylongdiskname:5":         "at most 20 characters",
		"Data:5, data:6":                      "duplicate disk name",
		"a:1,b:1,c:1,d:1,e:1,f:1,g:1,h:1,i:1": "at most 8 additional disks",
	}
	for in, want := range bad {
		_, err := ParseDisks(in)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("ParseDisks(%q) = %v, want error with %q", in, err, want)
		}
	}
}

func TestDiffDisks(t *testing.T) {
	old := []Disk{{"data", 50}, {"scratch", 10}, {"logs", 5}}
	cur := []Disk{{"data", 80}, {"logs", 5}, {"new", 1}}
	ch, err := DiffDisks(old, cur)
	if err != nil {
		t.Fatal(err)
	}
	want := DiskChange{Added: []Disk{{"new", 1}}, Grown: []Disk{{"data", 80}}, Removed: []Disk{{"scratch", 10}}}
	if !reflect.DeepEqual(ch, want) {
		t.Errorf("DiffDisks = %+v, want %+v", ch, want)
	}
	if !ch.Any() {
		t.Error("Any() should be true")
	}
	if ch, err := DiffDisks(old, old); err != nil || ch.Any() {
		t.Errorf("no change: %+v %v", ch, err)
	}
	_, err = DiffDisks(old, []Disk{{"data", 20}})
	if err == nil || !strings.Contains(err.Error(), `disk "data" can only grow (currently 50 GiB)`) {
		t.Errorf("shrink: %v", err)
	}
	if DiskNames(old) != "data, scratch, logs" {
		t.Errorf("DiskNames = %q", DiskNames(old))
	}
}

func TestBuildQEMUArgsExtraDisks(t *testing.T) {
	for _, arch := range []string{"x86_64", "aarch64"} {
		storage := filepath.Join(t.TempDir(), "vms, here") // comma: the option escaping must hold up
		cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Arch: arch, Network: NetworkConfig{Type: NetworkNone}}
		_, args, err := BuildQEMUArgs(cfg, storage)
		if err != nil {
			t.Fatal(err)
		}
		joined := strings.Join(args, " ")
		// Every VM gets the root ports, pinned to the same slots, disks or not.
		for i := 0; i < MaxExtraDisks; i++ {
			want := fmt.Sprintf("-device pcie-root-port,id=disk-rp%d,bus=pcie.0,chassis=%d,addr=0x%x", i+1, i+1, 0x10+i)
			if !strings.Contains(joined, want) {
				t.Errorf("%s: args lack %q:\n%s", arch, want, joined)
			}
		}
		if n := strings.Count(joined, "pcie-root-port"); n != MaxExtraDisks {
			t.Errorf("%s: %d root ports, want %d", arch, n, MaxExtraDisks)
		}
		if strings.Contains(joined, "virtio-blk-pci") {
			t.Errorf("%s: no extra disks configured, yet:\n%s", arch, joined)
		}

		cfg.Disks = []Disk{{"data", 50}, {"scratch", 10}}
		_, args, err = BuildQEMUArgs(cfg, storage)
		if err != nil {
			t.Fatal(err)
		}
		joined = strings.Join(args, " ")
		for i, d := range cfg.Disks {
			path := strings.ReplaceAll(ExtraDiskPath(storage, "t", d.Name), ",", ",,")
			want := fmt.Sprintf("-drive if=none,id=disk-%[1]s-drive,format=qcow2,file=%[2]s -device virtio-blk-pci,id=disk-%[1]s,drive=disk-%[1]s-drive,bus=disk-rp%[3]d,serial=%[1]s", d.Name, path, i+1)
			if !strings.Contains(joined, want) {
				t.Errorf("%s: args lack %q:\n%s", arch, want, joined)
			}
		}
		// The main disk comes first (the guest numbers it vda), the ports
		// before the devices that sit on them.
		if !(strings.Index(joined, "if=virtio") < strings.Index(joined, "pcie-root-port") &&
			strings.Index(joined, "id=disk-rp8") < strings.Index(joined, "virtio-blk-pci")) {
			t.Errorf("%s: order wrong:\n%s", arch, joined)
		}
	}

	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone}}
	for i := 0; i <= MaxExtraDisks; i++ {
		cfg.Disks = append(cfg.Disks, Disk{Name: "d" + strconv.Itoa(i), Size: 1})
	}
	if _, _, err := BuildQEMUArgs(cfg, t.TempDir()); err == nil || !strings.Contains(err.Error(), "at most 8") {
		t.Errorf("too many disks: %v", err)
	}
}

// TestStartRefusesMissingExtraDisk checks Start fails up front, with the path,
// rather than launching a QEMU that dies on its discarded stderr.
func TestStartRefusesMissingExtraDisk(t *testing.T) {
	storage := t.TempDir()
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone},
		Disks: []Disk{{"data", 1}}}
	if err := os.MkdirAll(VMDir(storage, cfg.Name), 0o755); err != nil {
		t.Fatal(err)
	}
	err := Start(storage, cfg)
	if err == nil || !strings.Contains(err.Error(), "data.qcow2") || !strings.Contains(err.Error(), "edit form") {
		t.Errorf("Start error = %v", err)
	}
	if info, _ := Status(storage, cfg.Name); info.Status != StatusStopped {
		t.Errorf("a VM must not be started with a missing disk: %+v", info)
	}
}

func TestCreateAndUpdateExtraDisks(t *testing.T) {
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		t.Skipf("%s not installed", qemuImgBin)
	}
	mgr := NewManager(t.TempDir())
	gib := func(n int) string { return strconv.Itoa(n << 30) }

	cfg := &VMConfig{Name: "deb", CPU: 1, RAM: 512, DiskSize: 1, Network: NetworkConfig{Type: NetworkNone},
		Disks: []Disk{{"data", 1}}}
	if err := mgr.Create(cfg); err != nil {
		t.Fatalf("Create: %v", err)
	}
	data := ExtraDiskPath(mgr.StoragePath, "deb", "data")
	if got := virtualSize(t, data); got != gib(1) {
		t.Errorf("data virtual size %s, want %s", got, gib(1))
	}
	loaded, err := LoadConfig(mgr.StoragePath, "deb")
	if err != nil || !reflect.DeepEqual(loaded.Disks, cfg.Disks) {
		t.Fatalf("saved disks = %+v, %v", loaded.Disks, err)
	}

	// Grow one, add one.
	cur := *loaded
	cur.Disks = []Disk{{"data", 2}, {"scratch", 1}}
	if err := mgr.Update("deb", &cur); err != nil {
		t.Fatalf("Update add+grow: %v", err)
	}
	scratch := ExtraDiskPath(mgr.StoragePath, "deb", "scratch")
	if got := virtualSize(t, data); got != gib(2) {
		t.Errorf("data not grown: %s", got)
	}
	if _, err := os.Stat(scratch); err != nil {
		t.Errorf("scratch not created: %v", err)
	}
	if loaded, _ = LoadConfig(mgr.StoragePath, "deb"); !reflect.DeepEqual(loaded.Disks, cur.Disks) {
		t.Errorf("saved disks = %+v", loaded.Disks)
	}

	// Shrinking is refused before anything is touched.
	shrunk := cur
	shrunk.Disks = []Disk{{"data", 1}, {"scratch", 1}}
	if err := mgr.Update("deb", &shrunk); err == nil || !strings.Contains(err.Error(), "can only grow") {
		t.Errorf("shrink: %v", err)
	}

	// A new disk never replaces a file that is already there.
	stray := ExtraDiskPath(mgr.StoragePath, "deb", "stray")
	if err := os.WriteFile(stray, []byte("precious"), 0o644); err != nil {
		t.Fatal(err)
	}
	over := cur
	over.Disks = append([]Disk{}, cur.Disks...)
	over.Disks = append(over.Disks, Disk{"stray", 1})
	if err := mgr.Update("deb", &over); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Errorf("create over existing file: %v", err)
	}
	if b, _ := os.ReadFile(stray); string(b) != "precious" {
		t.Errorf("stray file overwritten: %q", b)
	}
	if loaded, _ = LoadConfig(mgr.StoragePath, "deb"); !reflect.DeepEqual(loaded.Disks, cur.Disks) {
		t.Errorf("failed update changed the saved disks: %+v", loaded.Disks)
	}
	_ = os.Remove(stray)

	// While running: no growing or removing, but adding is fine.
	if err := os.WriteFile(PIDPath(mgr.StoragePath, "deb"), []byte(strconv.Itoa(os.Getpid())), 0o644); err != nil {
		t.Fatal(err)
	}
	grow := cur
	grow.Disks = []Disk{{"data", 3}, {"scratch", 1}}
	if err := mgr.Update("deb", &grow); err == nil || !strings.Contains(err.Error(), "stop the VM") || !strings.Contains(err.Error(), "data") {
		t.Errorf("grow while running: %v", err)
	}
	remove := cur
	remove.Disks = []Disk{{"data", 2}}
	if err := mgr.Update("deb", &remove); err == nil || !strings.Contains(err.Error(), "stop the VM") || !strings.Contains(err.Error(), "scratch") {
		t.Errorf("remove while running: %v", err)
	}
	if _, err := os.Stat(scratch); err != nil {
		t.Errorf("scratch removed despite the refusal: %v", err)
	}
	add := cur
	add.Disks = []Disk{{"data", 2}, {"scratch", 1}, {"logs", 1}}
	if err := mgr.Update("deb", &add); err != nil {
		t.Errorf("add while running: %v", err)
	}
	logs := ExtraDiskPath(mgr.StoragePath, "deb", "logs")
	if _, err := os.Stat(logs); err != nil {
		t.Errorf("logs not created: %v", err)
	}
	_ = os.Remove(PIDPath(mgr.StoragePath, "deb"))

	// Removing deletes the images; the others stay.
	remove = add
	remove.Disks = []Disk{{"data", 2}}
	if err := mgr.Update("deb", &remove); err != nil {
		t.Fatalf("remove: %v", err)
	}
	for _, p := range []string{scratch, logs} {
		if _, err := os.Stat(p); !os.IsNotExist(err) {
			t.Errorf("%s still there after removal (%v)", p, err)
		}
	}
	if _, err := os.Stat(data); err != nil {
		t.Errorf("data gone: %v", err)
	}

	// Renaming the VM takes the images along.
	renamed := remove
	renamed.Name = "deb2"
	if err := mgr.Update("deb", &renamed); err != nil {
		t.Fatalf("rename: %v", err)
	}
	if _, err := os.Stat(ExtraDiskPath(mgr.StoragePath, "deb2", "data")); err != nil {
		t.Errorf("data not moved with the VM: %v", err)
	}
	if loaded, err = LoadConfig(mgr.StoragePath, "deb2"); err != nil || !reflect.DeepEqual(loaded.Disks, []Disk{{"data", 2}}) {
		t.Errorf("renamed VM's disks = %+v, %v", loaded.Disks, err)
	}
}

func TestCreateCleansUpOnlyItsOwnDir(t *testing.T) {
	if _, err := exec.LookPath(qemuImgBin); err != nil {
		t.Skipf("%s not installed", qemuImgBin)
	}
	mgr := NewManager(t.TempDir())
	// A size qemu-img rejects, after the directory has been made.
	bad := &VMConfig{Name: "bad", CPU: 1, RAM: 128, DiskSize: -1, Network: NetworkConfig{Type: NetworkNone}}
	if err := mgr.Create(bad); err == nil {
		t.Fatal("create with a negative disk size succeeded")
	}
	if _, err := os.Stat(VMDir(mgr.StoragePath, "bad")); !os.IsNotExist(err) {
		t.Errorf("failed create left its directory behind (%v)", err)
	}

	// A directory that was already there, holding a file, is left alone.
	kept := VMDir(mgr.StoragePath, "kept")
	if err := os.MkdirAll(kept, 0o755); err != nil {
		t.Fatal(err)
	}
	note := filepath.Join(kept, "notes.txt")
	if err := os.WriteFile(note, []byte("mine"), 0o644); err != nil {
		t.Fatal(err)
	}
	bad.Name = "kept"
	if err := mgr.Create(bad); err == nil {
		t.Fatal("create succeeded")
	}
	if b, err := os.ReadFile(note); err != nil || string(b) != "mine" {
		t.Errorf("pre-existing directory was removed: %q %v", b, err)
	}

	// Duplicate names are caught before anything is made.
	dup := &VMConfig{Name: "dup", CPU: 1, RAM: 128, DiskSize: 1, Network: NetworkConfig{Type: NetworkNone},
		Disks: []Disk{{"data", 1}, {"Data", 1}}}
	if err := mgr.Create(dup); err == nil || !strings.Contains(err.Error(), "duplicate") {
		t.Errorf("duplicate disks: %v", err)
	}
	if _, err := os.Stat(VMDir(mgr.StoragePath, "dup")); !os.IsNotExist(err) {
		t.Error("directory made for an invalid config")
	}
}

func TestDiskHotplugHMP(t *testing.T) {
	storage := t.TempDir()
	cfg := &VMConfig{Name: "t", Disks: []Disk{{"data", 1}, {"scratch", 2}}}
	if err := os.MkdirAll(VMDir(storage, "t"), 0o755); err != nil {
		t.Fatal(err)
	}
	scratch := writeImage(t, VMDir(storage, "t"), "scratch.qcow2", 16) // only has to exist

	var (
		mu          sync.Mutex
		seen        []string
		deviceReply = ""
	)
	fakeHMPSessions(t, MonitorPath(storage, "t"), func(cmd string) string {
		mu.Lock()
		defer mu.Unlock()
		seen = append(seen, cmd)
		switch {
		case strings.HasPrefix(cmd, "drive_add "):
			return "OK\r\n"
		case strings.HasPrefix(cmd, "device_add "):
			return deviceReply
		}
		return ""
	})
	commands := func() []string {
		mu.Lock()
		defer mu.Unlock()
		out := seen
		seen = nil
		return out
	}

	if err := DiskHotplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-plug: %v", err)
	}
	want := []string{
		`drive_add 0 "if=none,id=disk-scratch-drive,format=qcow2,file=` + scratch + `"`,
		"device_add virtio-blk-pci,id=disk-scratch,drive=disk-scratch-drive,bus=disk-rp2,serial=scratch",
	}
	if got := commands(); !reflect.DeepEqual(got, want) {
		t.Errorf("monitor got\n%q\nwant\n%q", got, want)
	}

	// A missing image is caught before anything reaches the monitor.
	if err := DiskHotplug(storage, cfg, 0); err == nil || !strings.Contains(err.Error(), "not found") {
		t.Errorf("missing image: %v", err)
	}
	if got := commands(); len(got) != 0 {
		t.Errorf("monitor reached for a missing image: %q", got)
	}

	// QEMU's own error surfaces, and the drive added first is rolled back.
	mu.Lock()
	deviceReply = "Error: Bus 'disk-rp2' not found\r\n"
	mu.Unlock()
	err := DiskHotplug(storage, cfg, 1)
	if err == nil || !strings.Contains(err.Error(), "Bus 'disk-rp2' not found") {
		t.Errorf("device_add failure: %v", err)
	}
	if got := commands(); len(got) != 3 || got[2] != "drive_del disk-scratch-drive" {
		t.Errorf("no rollback after a failed device_add: %q", got)
	}

	if err := DiskHotplug(storage, cfg, 5); err == nil {
		t.Error("index past the configured disks accepted")
	}
}

// TestExtraDisksWithQEMU boots a real VM with one extra disk, hot-plugs a
// second one onto its root port and checks QEMU's view of both. Skipped
// without QEMU.
func TestExtraDisksWithQEMU(t *testing.T) {
	for _, bin := range []string{"qemu-system-x86_64", "qemu-img"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Skipf("%s not installed", bin)
		}
	}
	storage := t.TempDir()
	cfg := &VMConfig{
		Name: "disktest", CPU: 1, RAM: 128, DiskSize: 1,
		Network: NetworkConfig{Type: NetworkNone},
		Disks:   []Disk{{"data", 1}},
	}
	if err := NewManager(storage).Create(cfg); err != nil {
		t.Fatal(err)
	}
	if err := Start(storage, cfg); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = Stop(storage, cfg.Name) })

	waitForMonitor(t, storage, cfg.Name)
	block := waitForBlock(t, storage, cfg.Name, ExtraDiskPath(storage, cfg.Name, "data"), true)
	if !strings.Contains(block, "disk-data-drive") || !strings.Contains(block, "/disk-data/") {
		t.Errorf("boot-time disk not attached under its IDs:\n%s", block)
	}

	// Hot-plug a second disk, the way an edit of a running VM does: the
	// image is made first, then the device goes onto the next root port.
	cfg.Disks = append(cfg.Disks, Disk{"scratch", 1})
	scratch := ExtraDiskPath(storage, cfg.Name, "scratch")
	if err := createDiskImage(scratch, 1); err != nil {
		t.Fatal(err)
	}
	if err := DiskHotplug(storage, cfg, 1); err != nil {
		t.Fatalf("hot-plug: %v", err)
	}
	block = waitForBlock(t, storage, cfg.Name, scratch, true)
	if !strings.Contains(block, "/disk-scratch/") {
		t.Errorf("hot-plugged disk not attached to its device:\n%s", block)
	}
	qtree, _ := MonitorCommand(storage, cfg.Name, "info qtree")
	port := qtree[strings.Index(qtree, "bus: disk-rp2"):]
	if end := strings.Index(port[1:], "dev: pcie-root-port"); end > 0 {
		port = port[:end]
	}
	if !strings.Contains(port, `id "disk-scratch"`) || !strings.Contains(port, `serial = "scratch"`) {
		t.Errorf("hot-plugged disk should sit on disk-rp2 with its serial:\n%s", port)
	}

	// Plugging the same disk twice fails with QEMU's message, and the failed
	// attempt leaves no second drive behind.
	if err := DiskHotplug(storage, cfg, 1); err == nil || !strings.Contains(err.Error(), "disk-scratch") {
		t.Errorf("duplicate hot-plug should fail naming the disk, got: %v", err)
	}
	if block, _ = MonitorCommand(storage, cfg.Name, "info block"); strings.Count(block, scratch) != 1 {
		t.Errorf("failed hot-plug left a drive behind:\n%s", block)
	}

	if err := Stop(storage, cfg.Name); err != nil {
		t.Fatal(err)
	}
}
