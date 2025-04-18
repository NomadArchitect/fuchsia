// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffx

import (
	"context"
	"errors"
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"go.fuchsia.dev/fuchsia/src/connectivity/network/testing/conformance/util"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
)

const (
	PERM_USER_READ_WRITE_EXECUTE uint32 = 0700
)

type FfxInstance struct {
	outputDir string
	// If true, outputDir should be cleaned up
	// when FfxInstance is closed.
	iOwnOutputDir bool
	mu            struct {
		isClosed bool
		sync.Mutex
	}
	*ffxutil.FFXInstance
}

// Options for creating an FfxInstance.
type FfxInstanceOptions struct {
	// The Target for invocations of FFXInstance.RunWithTarget().
	// Empty string means no target is specified.
	Target string
	// The directory in which the isolated FFX config and daemon socket for this
	// instance should live. Empty string means that a random temp dir should be
	// created, and removed on FfxInstance.Close().
	TestOutputDir string
	// The path to the ffx binary. Empty string means that the default ffx
	// binary in the host out directory will be used.
	FfxBinPath string
}

// GetFfxPath returns the absolute path to the ffx binary.
func GetFfxPath() (string, error) {
	hostToolsDir, err := util.GetHostToolsDirectory()
	if err != nil {
		return "", fmt.Errorf("GetHostToolsDirectory() = %w", err)
	}
	return filepath.Join(hostToolsDir, "ffx"), nil
}

// NewFfxInstance returns a ffxutil.FFXInstance that executes against a
// QemuInstance running on the same machine.
func NewFfxInstance(
	ctx context.Context,
	options FfxInstanceOptions,
) (*FfxInstance, error) {
	ffx := options.FfxBinPath
	if ffx == "" {
		path, err := GetFfxPath()
		if err != nil {
			return nil, fmt.Errorf("getFfxPath() = %w", err)
		}
		ffx = path
	}

	fmt.Printf("os.Environ() = %s\n", os.Environ())

	wrapperFfxInstance := FfxInstance{outputDir: options.TestOutputDir}

	if wrapperFfxInstance.outputDir == "" {
		dir, err := os.MkdirTemp("", "ffx-instance-dir-*")
		if err != nil {
			return nil, fmt.Errorf(
				"os.MkdirTemp(\"\", \"ffx-instance-dir-*\") = %w",
				err,
			)
		}
		wrapperFfxInstance.outputDir = dir
		wrapperFfxInstance.iOwnOutputDir = true
	}

	// Configure the ssh keys to be created in the ffx instance dir
	sshKey := filepath.Join(wrapperFfxInstance.outputDir, "ssh_keys", "ssh_private_key")
	sshAuthKey := filepath.Join(wrapperFfxInstance.outputDir, "ssh_keys", "ssh_auth_keys")

	sshKeys := ffxutil.SSHInfo{SshPriv: sshKey, SshPub: sshAuthKey}

	// Set the ffx log dir relative to the FUCHSIA_TEST_OUT_DIR if present, that way
	// the logs are available in the CAS output for the test.
	logDir := os.Getenv("FUCHSIA_TEST_OUTDIR")
	if logDir == "" {
		logDir = wrapperFfxInstance.outputDir
	}
	logDir = filepath.Join(logDir, "ffx_logs")

	ffxInstance, err :=
		ffxutil.NewFFXInstance(
			ctx,
			ffx,
			// "dir" is the current directory of any subprocesses spun off by the
			// FFXInstance.
			/* dir= */
			"",
			// NewFFXInstance automatically inherits the current process's
			// os.Environ(), so we don't need to pass it in here.
			/* env= */
			[]string{},
			/* target= */ options.Target,
			&sshKeys,
			wrapperFfxInstance.outputDir,
			ffxutil.UseFFXLegacy,
			ffxutil.ConfigSettings{
				Level: "user",
				Settings: map[string]any{
					"log.dir": logDir,
				},
			},
		)
	if err != nil {
		return nil, fmt.Errorf("ffxutil.NewFFXInstance(..) = %w", err)
	}
	wrapperFfxInstance.FFXInstance = ffxInstance

	if err := wrapperFfxInstance.SetLogLevel(ctx, ffxutil.Warn); err != nil {
		return nil, fmt.Errorf("wrapperFfxInstance.SetLogLevel(%q) = %w", ffxutil.Warn, err)
	}

	fmt.Printf("====== Choosing FFX target: %s ======\n", options.Target)
	return &wrapperFfxInstance, nil
}

func (f *FfxInstance) IsClosed() bool {
	f.mu.Lock()
	defer f.mu.Unlock()

	return f.mu.isClosed
}

func (f *FfxInstance) Close() error {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.mu.isClosed = true

	err := f.Stop()
	if f.iOwnOutputDir {
		err = errors.Join(err, os.RemoveAll(f.outputDir))
	}

	if errors.Is(err, context.DeadlineExceeded) {
		// ffxutil sometimes returns a context.DeadlineExceeded error when stopping an FFXInstance when
		// the ffx daemon takes too long to shut down. This isn't really an actionable error, so it
		// makes sense to swallow it here.
		return nil
	}
	return err
}

const ffxIsolateDirKey = "FFX_ISOLATE_DIR="

func (f *FfxInstance) FfxIsolateDir() (string, error) {
	for _, envPair := range f.Env() {
		if strings.HasPrefix(envPair, ffxIsolateDirKey) {
			return envPair[len(ffxIsolateDirKey):], nil
		}
	}
	return "", fmt.Errorf("no FFX_ISOLATE_DIR in (%#v).Env()", f)
}

func (ffxInstance *FfxInstance) WaitUntilTargetIsAccessible(
	ctx context.Context,
	nodename string,
) error {
	if err := ffxInstance.TargetWait(ctx); err != nil {
		return fmt.Errorf(
			"Error while doing `ffx -t %s target wait`: %w",
			nodename,
			err,
		)
	}
	return nil
}

// CreateStdoutStderrTempFiles creates new files within the FfxInstance's output directory to write
// ffx's stdout and stderr to, returning the stdout and stderr files. The caller has
// responsibility for closing the os.Files returned.
func (f *FfxInstance) CreateStdoutStderrTempFiles() (*os.File, *os.File, error) {
	stdout, err := ioutil.TempFile(f.outputDir, "ffx-stdout-*.log")
	if err != nil {
		return nil, nil, err
	}

	stderr, err := ioutil.TempFile(f.outputDir, "ffx-stderr-*.log")
	if err != nil {
		return nil, nil, errors.Join(err, stdout.Close())
	}

	f.SetStdoutStderr(stdout, stderr)
	return stdout, stderr, nil
}
