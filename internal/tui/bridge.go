package tui

import "github.com/pzindyaev/ostrich/internal/vm"

// hostBridgeHint looks the bridge up on the host; tests swap it out.
var hostBridgeHint = vm.BridgeHint

// bridgeHintFor is the setup hint to show next to a network choice: only tap
// networking asks anything of the host, and "" means it is all there.
func bridgeHintFor(nt vm.NetworkType) string {
	if nt != vm.NetworkTap {
		return ""
	}
	return hostBridgeHint(vm.BridgeName)
}

// renderBridgeHint draws the hint as a warning block, commands and all.
func renderBridgeHint(hint string) string {
	return styleWarning.Render(indent("⚠ "+hint, "  ")) + "\n\n"
}
