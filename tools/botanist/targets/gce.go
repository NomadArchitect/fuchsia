// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"os/user"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/bootserver"
	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/lib/iomisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/retry"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"

	"golang.org/x/crypto/ssh"
)

const (
	gcemClientBinary = "./gcem_client"
)

// gceSerial is a ReadWriteCloser that talks to a GCE serial port via SSH.
type gceSerial struct {
	in     io.WriteCloser
	out    io.Reader
	sess   *ssh.Session
	client *ssh.Client
	closed bool
}

func newGCESerial(pkeyPath, username, endpoint string) (*gceSerial, error) {
	// Load the pkey and use it to dial the GCE serial port.
	data, err := os.ReadFile(pkeyPath)
	if err != nil {
		return nil, err
	}
	signer, err := ssh.ParsePrivateKey(data)
	if err != nil {
		return nil, err
	}
	sshConfig := &ssh.ClientConfig{
		User: username,
		Auth: []ssh.AuthMethod{
			ssh.PublicKeys(signer),
		},
		// TODO(rudymathu): Replace this with google ssh serial port key.
		HostKeyCallback: ssh.InsecureIgnoreHostKey(),
	}
	client, err := ssh.Dial("tcp", endpoint, sshConfig)
	if err != nil {
		return nil, err
	}

	// Create an SSH shell and wire up stdio.
	session, err := client.NewSession()
	if err != nil {
		return nil, err
	}
	out, err := session.StdoutPipe()
	if err != nil {
		return nil, err
	}
	in, err := session.StdinPipe()
	if err != nil {
		return nil, err
	}
	if err := session.Shell(); err != nil {
		return nil, err
	}
	return &gceSerial{
		in:     in,
		out:    out,
		sess:   session,
		client: client,
	}, nil
}

func (s *gceSerial) Read(b []byte) (int, error) {
	if s.closed {
		return 0, os.ErrClosed
	}
	return s.out.Read(b)
}

func (s *gceSerial) Write(b []byte) (int, error) {
	// Chunk out writes to 128 bytes or less. SSH connections to GCE do not
	// seem to properly handle longer messages.
	maxChunkSize := 128
	numChunks := len(b) / maxChunkSize
	if len(b)%maxChunkSize != 0 {
		numChunks++
	}
	bytesWritten := 0
	for i := 0; i < numChunks; i++ {
		start := i * maxChunkSize
		end := start + maxChunkSize
		if end > len(b) {
			end = len(b)
		}
		n, err := s.in.Write(b[start:end])
		bytesWritten += n
		if err != nil {
			return bytesWritten, err
		}
		time.Sleep(100 * time.Millisecond)
	}
	return bytesWritten, nil
}

func (s *gceSerial) Close() error {
	multierr := ""
	if err := s.in.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH session input pipe: %s, ", err)
	}
	if err := s.sess.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH session: %s, ", err)
	}
	if err := s.client.Close(); err != nil {
		multierr += fmt.Sprintf("failed to close serial SSH client: %s", err)
	}
	s.closed = true
	if multierr != "" {
		return errors.New(multierr)
	}
	return nil
}

// GCEConfig represents the on disk config used by botanist to launch a GCE
// instance.
type GCEConfig struct {
	// MediatorURL is the url of the GCE Mediator.
	MediatorURL string `json:"mediator_url"`
	// BuildID is the swarming task ID of the associated build.
	BuildID string `json:"build_id"`
	// CloudProject is the cloud project to create the GCE Instance in.
	CloudProject string `json:"cloud_project"`
	// SwarmingServer is the URL to the swarming server that fed us this
	// task.
	SwarmingServer string `json:"swarming_server"`
	// MachineShape is the shape of the instance we want to create.
	MachineShape string `json:"machine_shape"`
	// InstanceName is the name of the instance.
	InstanceName string `json:"instance_name"`
	// Zone is the cloud zone in which the instance lives.
	Zone string `json:"zone"`
}

// GCE represents a GCE VM running Fuchsia.
type GCE struct {
	*genericFuchsiaTarget
	config      GCEConfig
	currentUser string
	ipv4        net.IP
	loggerCtx   context.Context
	opts        Options
	serial      io.ReadWriteCloser
}

var _ FuchsiaTarget = (*GCE)(nil)

// createInstanceRes is returned by the gcem_client's create-instance
// subcommand. Its schema is determined by the CreateInstanceRes proto
// message in http://google3/turquoise/infra/gce_mediator/proto/mediator.proto.
type createInstanceRes struct {
	InstanceName string `json:"instanceName"`
	Zone         string `json:"zone"`
}

// NewGCE creates, starts, and connects to the serial console of a GCE VM.
func NewGCE(ctx context.Context, config GCEConfig, opts Options) (*GCE, error) {
	u, err := user.Current()
	if err != nil {
		return nil, err
	}
	g := &GCE{
		config:      config,
		currentUser: u.Username,
		loggerCtx:   ctx,
		opts:        opts,
	}

	if config.InstanceName == "" && config.Zone == "" {
		// If the instance has not been created, create it now.
		logger.Infof(ctx, "creating the GCE instance")
		expBackoff := retry.NewExponentialBackoff(15*time.Second, 2*time.Minute, 2)
		if err := retry.Retry(ctx, expBackoff, g.createInstance, nil); err != nil {
			return nil, err
		}
		logger.Infof(ctx, "successfully created GCE instance: Name: %s, Zone: %s", g.config.InstanceName, g.config.Zone)
	} else {
		// The instance has already been created, so add the SSH key to it.
		logger.Infof(ctx, "adding the SSH public key to GCE instance %s", g.config.InstanceName)
		expBackoff := retry.NewExponentialBackoff(15*time.Second, 2*time.Minute, 2)
		if err := retry.Retry(ctx, expBackoff, g.addSSHKey, nil); err != nil {
			return nil, err
		}
		logger.Infof(ctx, "successfully added SSH key")
	}

	// Connect to the serial line.
	logger.Infof(ctx, "setting up the serial connection to the GCE instance")
	expBackoff := retry.NewExponentialBackoff(15*time.Second, 2*time.Minute, 2)
	connectSerialErrs := make(chan error)
	defer close(connectSerialErrs)
	go logErrors(ctx, "connectToSerial()", connectSerialErrs)
	if err := retry.Retry(ctx, expBackoff, g.connectToSerial, connectSerialErrs); err != nil {
		return nil, err
	}
	logger.Infof(ctx, "successfully connected to serial")

	// If we're running a non-bringup configuration, we need to provision an SSH key.
	if opts.ExpectsSSH {
		if err := g.provisionSSHKey(ctx); err != nil {
			return nil, err
		}
	}
	base, err := newGenericFuchsia(ctx, "", "", []string{g.opts.SSHKey}, g.serial)
	if err != nil {
		return nil, err
	}
	g.genericFuchsiaTarget = base
	return g, nil
}

func logErrors(ctx context.Context, functionName string, errs <-chan error) {
	for {
		err, more := <-errs
		if err != nil {
			logger.Errorf(ctx, "%s failed: %s, retrying", functionName, err)
		}
		if !more {
			return
		}
	}
}

// Provisions an SSH key over the serial connection.
func (g *GCE) provisionSSHKey(ctx context.Context) error {
	if g.serial == nil {
		return fmt.Errorf("serial is not connected")
	}
	time.Sleep(2 * time.Minute)
	logger.Infof(g.loggerCtx, "provisioning SSH key over serial")
	p, err := os.ReadFile(g.opts.AuthorizedKey)
	if err != nil {
		return err
	}
	pubkey := string(p)
	pubkey = strings.TrimSuffix(pubkey, "\n")
	pubkey = fmt.Sprintf("\"%s %s\"", pubkey, g.currentUser)
	cmds := []serial.Command{
		{Cmd: []string{"echo", pubkey, ">", "/data/ssh/authorized_keys"}},
	}
	if err := serial.RunCommands(ctx, g.serial, cmds); err != nil {
		return err
	}
	// Call ReadUntilMatchString with a new ctx w/ a shorter timeout so it doesn't hang if the output is corrupted
	return retry.Retry(ctx, retry.WithMaxAttempts(retry.NewConstantBackoff(5*time.Second), 5), func() error {
		truncCtx, cancel := context.WithTimeout(ctx, 1*time.Second)
		_, readErr := iomisc.ReadUntilMatchString(truncCtx, g.serial, pubkey)
		cancel()
		if readErr == nil {
			logger.Infof(g.loggerCtx, "successfully provisioned SSH key")
		} else if errors.Is(readErr, truncCtx.Err()) {
			logger.Warningf(ctx, "ssh key is corrupted")
		} else {
			logger.Errorf(ctx, "unexpected error checking for ssh key: %s", readErr)
		}
		return readErr
	}, nil)
	return nil
}

func getSerialEndpoint(zone string) string {
	zoneParts := strings.Split(zone, "-")
	region := strings.ToLower(strings.Join(zoneParts[:len(zoneParts)-1], "-"))
	return fmt.Sprintf("%s-ssh-serialport.googleapis.com:9600", region)
}

func (g *GCE) connectToSerial() error {
	username := fmt.Sprintf(
		"%s.%s.%s.%s.%s",
		g.config.CloudProject,
		g.config.Zone,
		g.config.InstanceName,
		g.currentUser,
		"replay-from=0",
	)
	serial, err := newGCESerial(g.opts.SSHKey, username, getSerialEndpoint(g.config.Zone))
	g.serial = serial
	return err
}

func (g *GCE) addSSHKey() error {
	invocation := []string{
		gcemClientBinary,
		"add-ssh-key",
		"-host", g.config.MediatorURL,
		"-project", g.config.CloudProject,
		"-instance-name", g.config.InstanceName,
		"-zone", g.config.Zone,
		"-user", g.currentUser,
		"-pubkey", g.opts.AuthorizedKey,
	}
	logger.Infof(g.loggerCtx, "GCE Mediator client command: %s", invocation)
	cmd := exec.Command(invocation[0], invocation[1:]...)
	stdout, stderr, flush := botanist.NewStdioWriters(g.loggerCtx, "gce")
	defer flush()
	cmd.Stdout = stdout
	cmd.Stderr = stderr
	return cmd.Run()
}

func (g *GCE) createInstance() error {
	taskID := os.Getenv("SWARMING_TASK_ID")
	if taskID == "" {
		return errors.New("task did not specify SWARMING_TASK_ID")
	}

	invocation := []string{
		gcemClientBinary,
		"create-instance",
		"-host", g.config.MediatorURL,
		"-project", g.config.CloudProject,
		"-build-id", g.config.BuildID,
		"-task-id", taskID,
		"-swarming-host", g.config.SwarmingServer,
		"-machine-shape", g.config.MachineShape,
		"-user", g.currentUser,
		"-pubkey", g.opts.AuthorizedKey,
	}

	logger.Infof(g.loggerCtx, "GCE Mediator client command: %s", invocation)
	cmd := exec.Command(invocation[0], invocation[1:]...)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		return err
	}
	var res createInstanceRes
	if err := json.NewDecoder(stdout).Decode(&res); err != nil {
		return err
	}
	if err := cmd.Wait(); err != nil {
		return err
	}
	g.config.InstanceName = res.InstanceName
	g.config.Zone = res.Zone
	return nil
}

// generatePrivateKey generates a 2048 bit RSA private key, writes it to
// a temporary file, and returns the path to the key.
func generatePrivateKey() (string, error) {
	pkey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return "", err
	}
	f, err := os.CreateTemp("", "gce_pkey")
	if err != nil {
		return "", err
	}
	defer f.Close()
	pemBlock := &pem.Block{
		Type:    "RSA PRIVATE KEY",
		Headers: nil,
		Bytes:   x509.MarshalPKCS1PrivateKey(pkey),
	}
	return f.Name(), pem.Encode(f, pemBlock)
}

// generatePublicKey reads the private key at path pkey and generates a public
// key in Authorized Keys format. Returns the path to the public key file.
func generatePublicKey(pkeyFile string) (string, error) {
	if pkeyFile == "" {
		return "", errors.New("no private key file provided")
	}
	data, err := os.ReadFile(pkeyFile)
	if err != nil {
		return "", err
	}
	block, _ := pem.Decode(data)
	pkey, err := x509.ParsePKCS1PrivateKey(block.Bytes)
	if err != nil {
		return "", err
	}
	pubkey, err := ssh.NewPublicKey(pkey.Public())
	if err != nil {
		return "", err
	}
	f, err := os.CreateTemp("", "gce_pubkey")
	if err != nil {
		return "", err
	}
	defer f.Close()
	_, err = f.Write(ssh.MarshalAuthorizedKey(pubkey))
	return f.Name(), err
}

func (g *GCE) IPv4() (net.IP, error) {
	if g.ipv4 == nil {
		fqdn := fmt.Sprintf("%s.%s.c.%s.internal", g.config.InstanceName, g.config.Zone, g.config.CloudProject)
		addr, err := net.ResolveIPAddr("ip4", fqdn)
		if err != nil {
			logger.Infof(g.loggerCtx, "failed to resolve IPv4 of instance %s: %s", g.config.InstanceName, err)
			return nil, err
		}
		g.ipv4 = addr.IP
	}
	return g.ipv4, nil
}

// GCE targets don't have IPv6 addresses.
func (g *GCE) IPv6() (*net.IPAddr, error) {
	return nil, nil
}

func (g *GCE) Nodename() string {
	// TODO(rudymathu): fill in nodename
	return ""
}

func (g *GCE) Serial() io.ReadWriteCloser {
	return g.serial
}

func (g *GCE) SSHKey() string {
	return g.opts.SSHKey
}

func (g *GCE) UseProductBundles() bool {
	return false
}

func (g *GCE) Start(_ context.Context, _ []bootserver.Image, _ []string, _ string, _ bool) error {
	return nil
}

func (g *GCE) Stop() error {
	g.genericFuchsiaTarget.Stop()
	return g.serial.Close()
}

func (g *GCE) Wait(context.Context) error {
	return ErrUnimplemented
}

func (g *GCE) SSHClient() (*sshutil.Client, error) {
	addr, err := g.IPv4()
	if err != nil {
		return nil, err
	}

	return g.sshClient(&net.IPAddr{IP: addr}, "gce")
}

func (g *GCE) TestConfig(expectsSSH bool) (any, error) {
	return TargetInfo(g, expectsSSH, nil)
}
