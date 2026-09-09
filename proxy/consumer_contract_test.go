package main

import (
	"bufio"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func processRows(t *testing.T) [][]string {
	t.Helper()
	f, err := os.Open(filepath.Join("..", "testdata", "proxy-process-cases.tsv"))
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = f.Close() }()
	var rows [][]string
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		if line := scanner.Text(); line != "" && !strings.HasPrefix(line, "#") {
			rows = append(rows, strings.Split(line, "\t"))
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	return rows
}

func TestContractStartupRows(t *testing.T) {
	for _, row := range processRows(t) {
		if len(row) != 4 || row[0] != "startup" {
			continue
		}
		switch row[1] {
		case "no public path variables":
			if got := allowPaths("", ""); len(got) != 1 || got[0] != "/etc/vhrn/allowlist" {
				t.Errorf("startup row %#v paths=%#v", row, got)
			}
		case "partial local variables":
			_, err := localConfigFrom(func(key string) string {
				if key == "VHRN_BROKER_ADDR" {
					return "127.0.0.1:1"
				}
				return ""
			})
			if err == nil {
				t.Errorf("startup row %#v succeeded", row)
			}
		case "invalid local token":
			path := filepath.Join(t.TempDir(), "token")
			if err := os.WriteFile(path, []byte("invalid"), 0o600); err != nil {
				t.Fatal(err)
			}
			_, err := localConfigFrom(func(key string) string {
				return map[string]string{
					"VHRN_LOOPBACK_ALLOWLISTS": "a,b,c",
					"VHRN_BROKER_ADDR":         "127.0.0.1:1",
					"VHRN_BROKER_TOKEN_FILE":   path,
				}[key]
			})
			if err == nil {
				t.Errorf("startup row %#v succeeded", row)
			}
		case "listener address already bound":
			listener, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer listener.Close()
			if contractBinaryFails(t, listener.Addr().String(), nil) == nil {
				t.Errorf("startup row %#v succeeded", row)
			}
		case "broker readiness refused":
			listener, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			brokerAddress := listener.Addr().String()
			_ = listener.Close()
			token := filepath.Join(t.TempDir(), "token")
			if err := os.WriteFile(token, []byte(strings.Repeat("a", 64)), 0o600); err != nil {
				t.Fatal(err)
			}
			extra := map[string]string{"VHRN_LOOPBACK_ALLOWLISTS": "a,b,c", "VHRN_BROKER_ADDR": brokerAddress, "VHRN_BROKER_TOKEN_FILE": token}
			if contractBinaryFails(t, "127.0.0.1:0", extra) == nil {
				t.Errorf("startup row %#v succeeded", row)
			}
		}
	}
}

func contractBinaryFails(t *testing.T, address string, extra map[string]string) error {
	t.Helper()
	dir := t.TempDir()
	allow, mode := filepath.Join(dir, "allow"), filepath.Join(dir, "mode")
	if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(mode, []byte("enforce\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	binary := filepath.Join(dir, "vhrn-proxy")
	if output, err := exec.Command("go", "build", "-o", binary, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v: %s", err, output)
	}
	command := exec.Command(binary)
	command.Env = append(os.Environ(), "VHRN_ALLOWLIST="+allow, "VHRN_MODE_FILE="+mode, "VHRN_PROXY_LISTEN="+address)
	for key, value := range extra {
		command.Env = append(command.Env, key+"="+value)
	}
	if err := command.Start(); err != nil {
		return err
	}
	done := make(chan error, 1)
	go func() { done <- command.Wait() }()
	select {
	case err := <-done:
		return err
	case <-time.After(time.Second):
		_ = command.Process.Kill()
		return nil
	}
}

func TestContractLifecycleRow(t *testing.T) {
	for _, row := range processRows(t) {
		if len(row) != 4 || row[0] != "lifecycle" {
			continue
		}
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		address := listener.Addr().String()
		if err := listener.Close(); err != nil {
			t.Fatal(err)
		}
		dir := t.TempDir()
		allow, mode := filepath.Join(dir, "allow"), filepath.Join(dir, "mode")
		if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(mode, []byte("enforce\n"), 0o600); err != nil {
			t.Fatal(err)
		}
		binary := filepath.Join(dir, "vhrn-proxy")
		build := exec.Command("go", "build", "-o", binary, ".")
		if output, err := build.CombinedOutput(); err != nil {
			t.Fatalf("build: %v: %s", err, output)
		}
		command := exec.Command(binary)
		command.Env = append(os.Environ(), "VHRN_ALLOWLIST="+allow, "VHRN_MODE_FILE="+mode, "VHRN_PROXY_LISTEN="+address)
		if err := command.Start(); err != nil {
			t.Fatal(err)
		}
		defer func() { _ = command.Process.Kill() }()
		deadline := time.Now().Add(time.Second)
		for {
			connection, err := net.DialTimeout("tcp", address, 20*time.Millisecond)
			if err == nil {
				_ = connection.Close()
				break
			}
			if time.Now().After(deadline) {
				t.Fatalf("lifecycle row %#v did not listen", row)
			}
			time.Sleep(10 * time.Millisecond)
		}
		if err := command.Process.Signal(syscall.SIGTERM); err != nil {
			t.Fatal(err)
		}
		done := make(chan error, 1)
		go func() { done <- command.Wait() }()
		select {
		case err := <-done:
			if err == nil {
				t.Fatalf("lifecycle row %#v exited cleanly", row)
			}
		case <-time.After(time.Second):
			t.Fatalf("lifecycle row %#v did not terminate", row)
		}
		if row[1] != "SIGTERM" || row[2] != "terminate" {
			t.Fatalf("bad lifecycle row %#v", row)
		}
	}
}
