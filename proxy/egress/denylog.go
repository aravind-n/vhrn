package egress

import (
	"fmt"
	"log"
	"os"
	"sync"
	"time"
)

// DenyLog records denied destinations to the process log and, if configured, to
// an append-only file the host reads via `vhrn net denied`.
type DenyLog struct {
	path string
	mu   sync.Mutex
}

// NewDenyLog returns a DenyLog. An empty path logs only to the process log.
func NewDenyLog(path string) *DenyLog {
	return &DenyLog{path: path}
}

// Record notes that egress to host was denied (or would be, under report).
func (d *DenyLog) Record(host string, mode Mode) {
	log.Printf("deny %s (mode=%s)", host, mode)
	if d.path == "" {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	f, err := os.OpenFile(d.path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return
	}
	defer f.Close()
	fmt.Fprintf(f, "%s\t%s\n", time.Now().UTC().Format(time.RFC3339), host)
}
