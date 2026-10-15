package smol

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// Version is this SDK's release. The local runtime bundle it downloads is the
// one released with it, so the Go code and the native engine cannot drift.
const Version = "1.28.1"

// releases is where the per-platform runtime bundles are published, as assets
// of the smol release with the same version. Tests point it at a local server.
var releases = "https://github.com/smol-machines/smol/releases/download"

// bundleChecksums is the release asset listing each bundle's sha256.
const bundleChecksums = "go-runtime-checksums.sha256"

// runtimeAssets locates what the local engine needs on this host: the native
// core plus the boot helper, hypervisor libraries and guest rootfs it
// launches machines with.
type runtimeAssets struct {
	library        string
	bootBinary     string
	libDir         string
	agentRootfs    string
	agentRootfsTar string
}

func localPlatformSupported() bool {
	_, err := platformSlug()
	return err == nil
}

// platformSlug names the host's runtime bundle. The names match the smol
// npm platform packages.
func platformSlug() (string, error) {
	switch runtime.GOOS + "/" + runtime.GOARCH {
	case "darwin/arm64":
		return "darwin-arm64", nil
	case "linux/amd64":
		return "linux-x64", nil
	case "linux/arm64":
		return "linux-arm64", nil
	}
	return "", fmt.Errorf("no local runtime for %s/%s (supported: macOS Apple Silicon, Linux amd64/arm64); "+
		"the cloud target works on any platform", runtime.GOOS, runtime.GOARCH)
}

func libraryName() string {
	if runtime.GOOS == "darwin" {
		return "libsmol_go.dylib"
	}
	return "libsmol_go.so"
}

// resolveRuntimeAssets finds the runtime bundle: SMOLMACHINES_RUNTIME_DIR (an
// unpacked bundle, for development and air-gapped hosts), else the cached
// bundle for this SDK version, downloading it on first use unless
// SMOLMACHINES_NO_DOWNLOAD is set. The engine's own SMOLVM_BOOT_BINARY,
// SMOLVM_LIB_DIR, SMOLVM_AGENT_ROOTFS and SMOLVM_AGENT_ROOTFS_TAR still win
// over what the bundle holds.
func resolveRuntimeAssets() (*runtimeAssets, error) {
	dir := os.Getenv("SMOLMACHINES_RUNTIME_DIR")
	if dir == "" {
		var err error
		if dir, err = ensureBundle(); err != nil {
			return nil, err
		}
	}
	a := &runtimeAssets{library: filepath.Join(dir, libraryName()), libDir: dir}
	if _, err := os.Stat(a.library); err != nil {
		return nil, fmt.Errorf("runtime bundle %s has no %s", dir, libraryName())
	}
	if helper := filepath.Join(dir, "smol-vmm"); fileExists(helper) {
		a.bootBinary = helper
	}
	if rootfs := filepath.Join(dir, "agent-rootfs.tar"); fileExists(rootfs) {
		a.agentRootfsTar = rootfs
	}
	// The engine reads these itself, but a value set with os.Setenv after
	// start-up is not visible to it when cgo is off, so pass them on.
	for env, field := range map[string]*string{
		"SMOLVM_BOOT_BINARY":      &a.bootBinary,
		"SMOLVM_LIB_DIR":          &a.libDir,
		"SMOLVM_AGENT_ROOTFS":     &a.agentRootfs,
		"SMOLVM_AGENT_ROOTFS_TAR": &a.agentRootfsTar,
	} {
		if v := os.Getenv(env); v != "" {
			*field = v
		}
	}
	if a.agentRootfs != "" {
		a.agentRootfsTar = ""
	}
	return a, nil
}

func fileExists(path string) bool {
	info, err := os.Stat(path)
	return err == nil && !info.IsDir()
}

// cacheRoot is where fetched runtimes live, shared with the smol Rust SDK.
func cacheRoot() (string, error) {
	if dir := os.Getenv("SMOLMACHINES_CACHE_DIR"); dir != "" {
		return dir, nil
	}
	base, err := os.UserCacheDir()
	if err != nil {
		return "", fmt.Errorf("no cache directory for the local runtime; set SMOLMACHINES_CACHE_DIR: %w", err)
	}
	return filepath.Join(base, "smolmachines"), nil
}

// ensureBundle returns the unpacked runtime bundle for this SDK version,
// fetching and verifying it when it is not cached yet.
func ensureBundle() (string, error) {
	slug, err := platformSlug()
	if err != nil {
		return "", err
	}
	root, err := cacheRoot()
	if err != nil {
		return "", err
	}
	name := fmt.Sprintf("smol-go-runtime-%s-%s", Version, slug)
	dir := filepath.Join(root, "go", name)
	if fileExists(filepath.Join(dir, libraryName())) {
		return dir, nil
	}
	if v := os.Getenv("SMOLMACHINES_NO_DOWNLOAD"); v != "" && v != "0" {
		return "", fmt.Errorf("no local runtime at %s and downloads are disabled (SMOLMACHINES_NO_DOWNLOAD); "+
			"set SMOLMACHINES_RUNTIME_DIR to an unpacked %s bundle", dir, name)
	}
	archive := name + ".tar.gz"
	base := fmt.Sprintf("%s/v%s", releases, Version)
	data, err := fetch(base + "/" + archive)
	if err != nil {
		return "", err
	}
	sums, err := fetch(base + "/" + bundleChecksums)
	if err != nil {
		return "", err
	}
	if err := verify(archive, data, sums); err != nil {
		return "", err
	}
	if err := os.MkdirAll(filepath.Dir(dir), 0o755); err != nil {
		return "", err
	}
	// Unpack beside the destination and rename into place, so a process that
	// races this one never sees a half-written bundle.
	tmp, err := os.MkdirTemp(filepath.Dir(dir), name+".partial-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(tmp)
	if err := extract(data, tmp); err != nil {
		return "", fmt.Errorf("unpack %s: %w", archive, err)
	}
	if err := os.Rename(tmp, dir); err != nil && !fileExists(filepath.Join(dir, libraryName())) {
		return "", err
	}
	if !fileExists(filepath.Join(dir, libraryName())) {
		return "", fmt.Errorf("%s did not contain %s", archive, libraryName())
	}
	return dir, nil
}

func fetch(url string) ([]byte, error) {
	client := &http.Client{Timeout: 10 * time.Minute}
	resp, err := client.Get(url)
	if err != nil {
		return nil, fmt.Errorf("download %s: %w", url, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("download %s: HTTP %d", url, resp.StatusCode)
	}
	return io.ReadAll(resp.Body)
}

// verify checks data against the archive's line in a sha256sum listing.
func verify(archive string, data, sums []byte) error {
	sum := sha256.Sum256(data)
	got := hex.EncodeToString(sum[:])
	sc := bufio.NewScanner(bytes.NewReader(sums))
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) == 2 && strings.TrimPrefix(fields[1], "*") == archive {
			if fields[0] != got {
				return fmt.Errorf("%s checksum mismatch: release lists %s, download is %s", archive, fields[0], got)
			}
			return nil
		}
	}
	return fmt.Errorf("%s is not listed in %s", archive, bundleChecksums)
}

// extract unpacks a gzipped tar into dir, refusing entries that escape it.
func extract(data []byte, dir string) error {
	gz, err := gzip.NewReader(bytes.NewReader(data))
	if err != nil {
		return err
	}
	tr := tar.NewReader(gz)
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
		target := filepath.Join(dir, h.Name)
		if rel, err := filepath.Rel(dir, target); err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			return fmt.Errorf("entry %q escapes the bundle", h.Name)
		}
		switch h.Typeflag {
		case tar.TypeDir:
			if err := os.MkdirAll(target, 0o755); err != nil {
				return err
			}
		case tar.TypeReg:
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			f, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, os.FileMode(h.Mode).Perm())
			if err != nil {
				return err
			}
			_, err = io.Copy(f, tr)
			if cerr := f.Close(); err == nil {
				err = cerr
			}
			if err != nil {
				return err
			}
		case tar.TypeSymlink:
			if filepath.IsAbs(h.Linkname) || strings.Contains(h.Linkname, "..") {
				return fmt.Errorf("symlink %q -> %q escapes the bundle", h.Name, h.Linkname)
			}
			if err := os.Symlink(h.Linkname, target); err != nil {
				return err
			}
		}
	}
}
