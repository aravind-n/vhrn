package vhrn

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
)

// syncClaudeSubdir mirrors one ~/.claude subdir into the sandbox, dereferencing
// symlinks (rsync -aL --delete, with a cp -RL fallback). --delete is confined to
// the subdir, so top-level sandbox files are never pruned.
func syncClaudeSubdir(real, sandbox, name string) {
	src := filepath.Join(real, name)
	if !dirExists(src) {
		return
	}
	dst := filepath.Join(sandbox, name)
	if lookPath("rsync") {
		if err := exec.Command("rsync", "-aL", "--delete", src+"/", dst+"/").Run(); err != nil {
			warnSkipped(name)
		}
		return
	}
	os.RemoveAll(dst)
	if err := exec.Command("cp", "-RL", src, dst).Run(); err != nil {
		warnSkipped(name)
	}
}

// copyFileInto copies a single ~/.claude file into the sandbox (cp -L).
func copyFileInto(real, sandbox, name string) {
	src := filepath.Join(real, name)
	if !fileExists(src) {
		return
	}
	if err := copyFile(src, filepath.Join(sandbox, name)); err != nil {
		fmt.Fprintf(os.Stderr, "vhrn: warning: could not copy '%s'\n", name)
	}
}

// copyFile copies src to dst, following symlinks in src (like cp -L).
func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return err
	}
	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

func warnSkipped(name string) {
	fmt.Fprintf(os.Stderr, "vhrn: warning: some '%s' entries were skipped (broken symlink?)\n", name)
}

func fileExists(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && !fi.IsDir()
}

func dirExists(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && fi.IsDir()
}
