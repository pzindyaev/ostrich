package vm

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseUSBID(t *testing.T) {
	d, err := ParseUSBID(" 046D:085c ")
	if err != nil {
		t.Fatal(err)
	}
	if d.VendorID != "046d" || d.ProductID != "085c" || d.ID() != "046d:085c" {
		t.Fatalf("got %+v", d)
	}
	for _, bad := range []string{"", "046d", "046d:85c", "46d:085c", "xyz1:0001", "046d:085c:1"} {
		if _, err := ParseUSBID(bad); err == nil {
			t.Errorf("ParseUSBID(%q) accepted", bad)
		}
	}
}

func TestUSBDeviceValidate(t *testing.T) {
	ok := USBDevice{VendorID: "046d", ProductID: "085c", Port: "3-2.2.2"}
	if err := ok.Validate(); err != nil {
		t.Fatal(err)
	}
	for _, bad := range []USBDevice{
		{VendorID: "46d", ProductID: "085c"},
		{VendorID: "046d", ProductID: "085c", Port: "2.2.2"},
		{VendorID: "046d", ProductID: "085c", Port: "3-"},
	} {
		if err := bad.Validate(); err == nil {
			t.Errorf("%+v accepted", bad)
		}
	}
}

func TestUSBDeviceIDsAndSpec(t *testing.T) {
	devs := []USBDevice{
		{VendorID: "046D", ProductID: "085c"},
		{VendorID: "046d", ProductID: "085c", Port: "3-2.2.2"},
		{VendorID: "046d", ProductID: "085c"}, // hand-edited duplicate
	}
	ids := USBDeviceIDs(devs)
	want := []string{"usb-046d-085c", "usb-046d-085c-3-2.2.2", "usb-046d-085c-2"}
	for i := range want {
		if ids[i] != want[i] {
			t.Errorf("id[%d] = %q, want %q", i, ids[i], want[i])
		}
	}

	spec := usbHostDevice(devs[0], ids[0])
	if spec != "usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c" {
		t.Errorf("spec = %q", spec)
	}
	spec = usbHostDevice(devs[1], ids[1])
	if !strings.HasSuffix(spec, ",hostbus=3,hostport=2.2.2") {
		t.Errorf("pinned spec = %q", spec)
	}
}

func TestBuildQEMUArgsUSB(t *testing.T) {
	cfg := &VMConfig{Name: "t", CPU: 1, RAM: 128, Network: NetworkConfig{Type: NetworkNone},
		USBDevices: []USBDevice{{VendorID: "046d", ProductID: "085c"}}}
	_, args := BuildQEMUArgs(cfg, t.TempDir())
	joined := strings.Join(args, " ")
	if !strings.Contains(joined, "-device qemu-xhci,id=xhci") {
		t.Errorf("missing xhci controller: %s", joined)
	}
	if !strings.Contains(joined, "-device usb-host,id=usb-046d-085c,bus=xhci.0,vendorid=0x046d,productid=0x085c") {
		t.Errorf("missing usb-host device: %s", joined)
	}
	// The controller is there even with no devices, so hot-plug always works.
	_, args = BuildQEMUArgs(&VMConfig{Name: "t", CPU: 1, RAM: 128}, t.TempDir())
	if !strings.Contains(strings.Join(args, " "), "qemu-xhci") {
		t.Error("xhci controller should be unconditional")
	}
}

func TestHostUSBDeviceLabel(t *testing.T) {
	cases := []struct {
		man, prod, want string
	}{
		{"Logitech", "USB Receiver", "Logitech USB Receiver"},
		{"Logitech", "Logitech G600", "Logitech G600"},
		{"", "C922 Pro Stream Webcam", "C922 Pro Stream Webcam"},
		{"FIIO", "", "FIIO"},
		{"", "", "1234:5678"},
	}
	for _, c := range cases {
		h := HostUSBDevice{VendorID: "1234", ProductID: "5678", Manufacturer: c.man, Product: c.prod}
		if got := h.Label(); got != c.want {
			t.Errorf("Label(%q, %q) = %q, want %q", c.man, c.prod, got, c.want)
		}
	}
}

// writeSysfsDevice creates a fake sysfs device directory.
func writeSysfsDevice(t *testing.T, root, name string, attrs map[string]string) {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	for k, v := range attrs {
		if err := os.WriteFile(filepath.Join(dir, k), []byte(v+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
}

func TestListHostUSBDevices(t *testing.T) {
	sys, dev := t.TempDir(), t.TempDir()
	usbSysfsDir, usbDevDir = sys, dev
	t.Cleanup(func() { usbSysfsDir, usbDevDir = "/sys/bus/usb/devices", "/dev/bus/usb" })

	writeSysfsDevice(t, sys, "usb3", map[string]string{"idVendor": "1d6b", "idProduct": "0002", "busnum": "3", "devnum": "1", "bDeviceClass": "09"})
	writeSysfsDevice(t, sys, "3-2", map[string]string{"idVendor": "174c", "idProduct": "2074", "busnum": "3", "devnum": "2", "bDeviceClass": "09", "product": "ASM107x"})
	writeSysfsDevice(t, sys, "3-2.10", map[string]string{"idVendor": "046d", "idProduct": "c52b", "busnum": "3", "devnum": "17", "bDeviceClass": "00", "manufacturer": "Logitech", "product": "USB Receiver"})
	writeSysfsDevice(t, sys, "3-2.2", map[string]string{"idVendor": "046d", "idProduct": "085c", "busnum": "3", "devnum": "16", "bDeviceClass": "ef", "product": "C922 Pro Stream Webcam"})
	writeSysfsDevice(t, sys, "3-2.2:1.0", map[string]string{"bInterfaceClass": "0e"})
	writeSysfsDevice(t, sys, "1-5", map[string]string{"idVendor": "0b05", "idProduct": "18f3", "busnum": "1", "devnum": "3", "bDeviceClass": "00", "manufacturer": "AsusTek Computer Inc.", "product": "AURA LED Controller"})

	// Only the webcam's device node exists and is writable.
	if err := os.MkdirAll(filepath.Join(dev, "003"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dev, "003", "016"), nil, 0o600); err != nil {
		t.Fatal(err)
	}

	devs, err := ListHostUSBDevices()
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, d := range devs {
		got = append(got, d.Port)
	}
	// Hubs and interfaces skipped; ordered by bus then port numerically (2 before 10).
	if want := "1-5 3-2.2 3-2.10"; strings.Join(got, " ") != want {
		t.Fatalf("ports = %q, want %q", strings.Join(got, " "), want)
	}
	if cam := devs[1]; !cam.Writable || cam.Label() != "C922 Pro Stream Webcam" || cam.ID() != "046d:085c" ||
		cam.DevNode != filepath.Join(dev, "003", "016") {
		t.Errorf("webcam = %+v", cam)
	}
	if devs[2].Writable {
		t.Error("device without a node must not be writable")
	}

	states := USBStates([]USBDevice{{VendorID: "046d", ProductID: "c52b"}, {VendorID: "dead", ProductID: "beef"}})
	if states[0].Host == nil || states[0].Host.Port != "3-2.10" || states[1].Host != nil {
		t.Errorf("states = %+v", states)
	}
	err = CheckUSBAccess([]USBDevice{{VendorID: "046d", ProductID: "085c"}})
	if err != nil {
		t.Errorf("writable device must pass: %v", err)
	}
	err = CheckUSBAccess([]USBDevice{{VendorID: "046d", ProductID: "c52b"}, {VendorID: "dead", ProductID: "beef"}})
	if err == nil || !strings.Contains(err.Error(), `'ATTR{idVendor}=="046d",' 'ATTR{idProduct}=="c52b",'`) ||
		strings.Contains(err.Error(), "dead") {
		t.Errorf("CheckUSBAccess error = %v", err)
	}
}

func TestMatchUSB(t *testing.T) {
	host := []HostUSBDevice{
		{VendorID: "0781", ProductID: "5583", Port: "1-1"},
		{VendorID: "0781", ProductID: "5583", Port: "1-2"},
		{VendorID: "046d", ProductID: "085c", Port: "1-3"},
	}
	devs := []USBDevice{
		{VendorID: "0781", ProductID: "5583"},              // unpinned: must not steal 1-2
		{VendorID: "0781", ProductID: "5583", Port: "1-2"}, // pinned
		{VendorID: "046D", ProductID: "085C", Port: "1-9"}, // pinned to an absent port
	}
	states := MatchUSB(devs, host)
	if states[0].Host == nil || states[0].Host.Port != "1-1" {
		t.Errorf("unpinned entry got %+v", states[0].Host)
	}
	if states[1].Host == nil || states[1].Host.Port != "1-2" {
		t.Errorf("pinned entry got %+v", states[1].Host)
	}
	if states[2].Host != nil {
		t.Errorf("entry pinned to absent port matched %+v", states[2].Host)
	}
	if &host[0] != states[0].Host {
		t.Error("Host must point into the slice passed in")
	}
}

func TestUdevRuleCommand(t *testing.T) {
	one := UdevRuleCommand(USBDevice{VendorID: "046D", ProductID: "085c"})
	want := `echo 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",' 'ATTR{idProduct}=="085c",' 'TAG+="uaccess"' ` +
		`| sudo tee -a /etc/udev/rules.d/70-ostrich-usb.rules && sudo udevadm control --reload && sudo udevadm trigger`
	if one != want {
		t.Errorf("one device:\n got %s\nwant %s", one, want)
	}
	two := UdevRuleCommand(USBDevice{VendorID: "046d", ProductID: "085c"}, USBDevice{VendorID: "0781", ProductID: "5583"})
	if !strings.HasPrefix(two, `printf '%s %s %s %s\n' 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="046d",'`) ||
		!strings.Contains(two, `'TAG+="uaccess"' 'SUBSYSTEM=="usb",' 'ATTR{idVendor}=="0781",'`) {
		t.Errorf("two devices: %s", two)
	}
}

// TestUdevRuleCommandProducesRules runs the generated pipeline (minus sudo) in a
// real shell and checks the file ends up with one well-formed rule per device.
func TestUdevRuleCommandProducesRules(t *testing.T) {
	for _, shell := range []string{"sh", "bash", "fish"} {
		sh, err := exec.LookPath(shell)
		if err != nil {
			continue
		}
		for n, devs := range [][]USBDevice{
			{{VendorID: "046d", ProductID: "085c"}},
			{{VendorID: "046d", ProductID: "085c"}, {VendorID: "0781", ProductID: "5583"}},
		} {
			rules := filepath.Join(t.TempDir(), "rules")
			words := UdevRuleCommandWords(devs...)
			// Keep only the part that writes the file: "<producer> | sudo tee -a FILE".
			words = words[:indexOf(words, "&&")]
			cmd := strings.ReplaceAll(strings.Join(words, " "), "sudo tee -a "+UdevRulesFile, "tee -a "+rules)
			// Break the line between words too, as the TUI does.
			cmd = strings.Replace(cmd, " 'ATTR{idProduct}", " \\\n  'ATTR{idProduct}", 1)
			if out, err := exec.Command(sh, "-c", cmd).CombinedOutput(); err != nil {
				t.Fatalf("%s (%d devices): %v\n%s", shell, len(devs), err, out)
			}
			got, _ := os.ReadFile(rules)
			var want strings.Builder
			for _, d := range devs {
				fmt.Fprintf(&want, "SUBSYSTEM==\"usb\", ATTR{idVendor}==\"%s\", ATTR{idProduct}==\"%s\", TAG+=\"uaccess\"\n", d.VendorID, d.ProductID)
			}
			if string(got) != want.String() {
				t.Errorf("%s (case %d) wrote:\n%s\nwant:\n%s", shell, n, got, want.String())
			}
		}
	}
}

func indexOf(words []string, w string) int {
	for i, x := range words {
		if x == w {
			return i
		}
	}
	return len(words)
}
