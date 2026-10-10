package config

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestRecentISOs(t *testing.T) {
	t.Setenv("HOME", t.TempDir())

	// Nothing to remember into before setup.
	if got := RecentISOs(); got != nil {
		t.Errorf("RecentISOs without a config = %v", got)
	}
	if err := RememberISO("/a.iso"); err != ErrNotFound {
		t.Errorf("RememberISO without a config: %v", err)
	}

	if err := Save(&AppConfig{VMStoragePath: "/vms"}); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{"/a.iso", "/b.iso", "/a.iso"} {
		if err := RememberISO(p); err != nil {
			t.Fatal(err)
		}
	}
	// Newest first, no duplicates.
	if got, want := RecentISOs(), []string{"/a.iso", "/b.iso"}; !reflect.DeepEqual(got, want) {
		t.Errorf("RecentISOs = %v, want %v", got, want)
	}
	// The storage path survives the rewrite.
	if cfg, err := Load(); err != nil || cfg.VMStoragePath != "/vms" {
		t.Errorf("config after remembering: %+v %v", cfg, err)
	}

	if err := ForgetISO("/a.iso"); err != nil {
		t.Fatal(err)
	}
	if err := ForgetISO("/never.iso"); err != nil {
		t.Fatal(err)
	}
	if got, want := RecentISOs(), []string{"/b.iso"}; !reflect.DeepEqual(got, want) {
		t.Errorf("after forget = %v, want %v", got, want)
	}

	// Only the newest maxRecentISOs are kept.
	for i := 0; i < maxRecentISOs+5; i++ {
		if err := RememberISO(fmt.Sprintf("/%d.iso", i)); err != nil {
			t.Fatal(err)
		}
	}
	got := RecentISOs()
	if len(got) != maxRecentISOs || got[0] != fmt.Sprintf("/%d.iso", maxRecentISOs+4) || got[len(got)-1] != "/5.iso" {
		t.Errorf("capped list = %v", got)
	}

	// The list is omitted from the file when empty.
	if err := Save(&AppConfig{VMStoragePath: "/vms"}); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filepath.Join(os.Getenv("HOME"), ".config", "ostrich", "config.json"))
	if string(data) != "{\n  \"vm_storage_path\": \"/vms\"\n}" {
		t.Errorf("config file:\n%s", data)
	}
}
