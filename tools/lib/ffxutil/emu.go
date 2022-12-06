// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffxutil

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
)

const (
	// The path to the SDK manifest relative to the sdk.root.
	SDKManifestPath = "sdk/manifest/core"
	// The path to the default virtual_device config that `ffx emu`
	// uses relative to the sdk.root.
	VirtualDevicePath = "virtual_device_recommended.json"
)

// EmuTools represent tools used by `ffx emu`. If using tools not included in the SDK,
// their paths should be provided in this struct to EmuStart().
type EmuTools struct {
	Emulator string
	FVM      string
	ZBI      string
}

// SDKManifest contains the atoms that are part of the "SDK" which ffx looks up to find
// the tools it needs to launch an emulator. The manifest should only contain references
// to files that exist.
type SDKManifest struct {
	Atoms []Atom `json:"atoms"`
}

type Atom struct {
	Files []File `json:"files"`
	ID    string `json:"id"`
	Meta  string `json:"meta"`
}

type File struct {
	Destination string `json:"destination"`
	Source      string `json:"source"`
}

// EmuStartConsole returns a command to launch the emulator.
func (f *FFXInstance) EmuStartConsole(ctx context.Context, sdkRoot, name string, qemu bool, config string, tools EmuTools) (*exec.Cmd, error) {
	// If using different tools from the ones in the sdk, `ffx emu` expects them to
	// have certain names and to be located in a parent directory of the ffx binary.
	ffxDir := filepath.Dir(f.ffxPath)
	toolsToSymlink := make(map[string]string)
	if tools.Emulator != "" {
		expectedName := "aemu_internal"
		if qemu {
			expectedName = "qemu_internal"
		}
		toolsToSymlink[tools.Emulator] = filepath.Join(ffxDir, expectedName)
	}
	if tools.FVM != "" {
		toolsToSymlink[tools.FVM] = filepath.Join(ffxDir, "fvm")
	}
	if tools.ZBI != "" {
		toolsToSymlink[tools.ZBI] = filepath.Join(ffxDir, "zbi")
	}
	for oldname, newname := range toolsToSymlink {
		if oldname == newname {
			continue
		}
		if err := os.Symlink(oldname, newname); err != nil && !os.IsExist(err) {
			return nil, err
		}
	}
	if err := f.ConfigSet(ctx, "sdk.type", "in-tree"); err != nil {
		return nil, err
	}
	absPath, err := filepath.Abs(sdkRoot)
	if err != nil {
		return nil, err
	}
	if err := f.ConfigSet(ctx, "sdk.root", absPath); err != nil {
		return nil, err
	}
	args := []string{"emu", "start", "--console", "--net", "tap", "--name", name, "-H", "-s", "0", "--config", config}
	if qemu {
		args = append(args, "--engine", "qemu")
	}
	return f.Command(args...), nil
}

// EmuStop terminates all emulator instances launched by ffx.
func (f *FFXInstance) EmuStop(ctx context.Context) error {
	return f.Run(ctx, "emu", "stop", "--all")
}

// GetEmuDeps returns the list of file dependencies for `ffx emu` to work.
func GetEmuDeps(sdkRoot string, targetCPU string, tools []string) ([]string, error) {
	deps := []string{
		SDKManifestPath,
		VirtualDevicePath,
		"product_bundle.json",
		"virtual_device_min.json",
		"obj/build/images/flash/virtual_device_specification_recommended_flags.json.template",
	}
	if targetCPU == "x64" {
		if _, err := os.Stat(filepath.Join(sdkRoot, "physical_device.json")); err == nil {
			deps = append(deps, "physical_device.json")
		}
	}

	manifestPath := filepath.Join(sdkRoot, SDKManifestPath)
	manifest, err := GetFFXEmuManifest(manifestPath, targetCPU, tools)
	if err != nil {
		return nil, err
	}

	for _, atom := range manifest.Atoms {
		for _, file := range atom.Files {
			deps = append(deps, file.Source)
		}
	}
	return deps, nil
}

// GetFFXEmuManifest returns an SDK manifest with the minimum number of atoms
// required by `ffx emu`. The `tools` are the names of the tools that we expect to
// use from the SDK.
func GetFFXEmuManifest(manifestPath, targetCPU string, tools []string) (SDKManifest, error) {
	var manifest SDKManifest
	if err := jsonutil.ReadFromFile(manifestPath, &manifest); err != nil {
		return manifest, fmt.Errorf("failed to read sdk manifest: %w", err)
	}
	if len(tools) == 0 {
		manifest.Atoms = []Atom{}
		return manifest, nil
	}

	toolIds := make(map[string]struct{})
	for _, tool := range tools {
		toolIds[fmt.Sprintf("sdk://tools/%s/%s", targetCPU, tool)] = struct{}{}
	}

	requiredAtoms := []Atom{}
	for _, atom := range manifest.Atoms {
		if _, ok := toolIds[atom.ID]; !ok {
			continue
		}
		requiredAtoms = append(requiredAtoms, atom)
	}
	manifest.Atoms = requiredAtoms
	return manifest, nil
}

// VirtualDevice represents the schema of the virtual_device specification config
// used by `ffx emu`. It should be kept in sync with build/sdk/virtual_device.gni.
type VirtualDevice struct {
	Data     VirtualDeviceData `json:"data"`
	SchemaID string            `json:"schema_id"`
}

type VirtualDeviceData struct {
	Description         string                `json:"description"`
	Hardware            VirtualDeviceHardware `json:"hardware"`
	Name                string                `json:"name"`
	Ports               map[string]int        `json:"ports"`
	StartUpArgsTemplate string                `json:"start_up_args_template"`
	Type                string                `json:"type"`
}

type VirtualDeviceHardware struct {
	Audio      map[string]string `json:"audio"`
	CPU        map[string]string `json:"cpu"`
	Inputs     map[string]string `json:"inputs"`
	Memory     quantity          `json:"memory"`
	Storage    quantity          `json:"storage"`
	WindowSize dimension         `json:"window_size"`
}

type quantity struct {
	Quantity int    `json:"quantity"`
	Units    string `json:"units"`
}

type dimension struct {
	Height int    `json:"height"`
	Units  string `json:"units"`
	Width  int    `json:"width"`
}

// GetVirtualDevice returns the contents of the virtual_device config.
func GetVirtualDevice(path string) (VirtualDevice, error) {
	var device VirtualDevice
	if err := jsonutil.ReadFromFile(path, &device); err != nil {
		return device, fmt.Errorf("failed to read %s: %w", path, err)
	}
	return device, nil
}
