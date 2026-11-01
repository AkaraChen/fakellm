package fakellm

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"runtime"
	"strings"
)

// ReleaseBaseURL is where Download looks for release assets. NOTE: no release has been
// published yet, so the default URL does not exist until the first v* tag is pushed.
var ReleaseBaseURL = "https://github.com/AkaraChen/fakellm/releases/download"

func resolveBin(explicit string) (string, error) {
	if explicit != "" {
		return explicit, nil
	}
	if env := os.Getenv("FAKELLM_BIN"); env != "" {
		return env, nil
	}
	if p, err := exec.LookPath("fakellm"); err == nil {
		return p, nil
	}
	return "", errors.New("fakellm: binary not found: set Options.Bin or $FAKELLM_BIN, put fakellm on $PATH, or call Download")
}

// Download fetches fakellm-<arch>-unknown-linux-musl.tar.gz for the given version (without the
// leading "v") from ReleaseBaseURL into cacheDir and returns the path of the extracted binary.
// An existing cached binary is reused. If a .sha256 file is published next to the tarball it is
// verified. Linux only (amd64 and arm64).
func Download(ctx context.Context, version, cacheDir string) (string, error) {
	if runtime.GOOS != "linux" {
		return "", fmt.Errorf("fakellm: Download supports linux only, not %s", runtime.GOOS)
	}
	var arch string
	switch runtime.GOARCH {
	case "amd64":
		arch = "x86_64"
	case "arm64":
		arch = "aarch64"
	default:
		return "", fmt.Errorf("fakellm: no release for GOARCH %s", runtime.GOARCH)
	}
	version = strings.TrimPrefix(version, "v")
	dir := filepath.Join(cacheDir, version+"-"+arch)
	dest := filepath.Join(dir, "fakellm")
	if st, err := os.Stat(dest); err == nil && st.Mode().IsRegular() {
		return dest, nil
	}
	url := fmt.Sprintf("%s/v%s/fakellm-%s-unknown-linux-musl.tar.gz", ReleaseBaseURL, version, arch)
	body, err := fetch(ctx, url)
	if err != nil {
		return "", err
	}
	defer body.Close()
	data, err := io.ReadAll(body)
	if err != nil {
		return "", fmt.Errorf("fakellm: download %s: %w", url, err)
	}
	if sum, err := fetch(ctx, url+".sha256"); err == nil {
		want, _ := io.ReadAll(sum)
		sum.Close()
		got := sha256.Sum256(data)
		if f := strings.Fields(string(want)); len(f) > 0 && !strings.EqualFold(f[0], hex.EncodeToString(got[:])) {
			return "", fmt.Errorf("fakellm: checksum mismatch for %s", url)
		}
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	if err := extractBinary(data, dest); err != nil {
		return "", fmt.Errorf("fakellm: extract %s: %w", url, err)
	}
	return dest, nil
}

func fetch(ctx context.Context, url string) (io.ReadCloser, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("fakellm: download %s: %w", url, err)
	}
	if res.StatusCode != http.StatusOK {
		res.Body.Close()
		return nil, fmt.Errorf("fakellm: download %s: %s", url, res.Status)
	}
	return res.Body, nil
}

// extractBinary writes the first regular file named "fakellm" in the tarball to dest, atomically.
func extractBinary(data []byte, dest string) error {
	gz, err := gzip.NewReader(strings.NewReader(string(data)))
	if err != nil {
		return err
	}
	tr := tar.NewReader(gz)
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			return errors.New("no fakellm binary in archive")
		}
		if err != nil {
			return err
		}
		if h.Typeflag != tar.TypeReg || path.Base(h.Name) != "fakellm" {
			continue
		}
		tmp, err := os.CreateTemp(filepath.Dir(dest), ".fakellm-*")
		if err != nil {
			return err
		}
		if _, err := io.Copy(tmp, tr); err != nil {
			tmp.Close()
			os.Remove(tmp.Name())
			return err
		}
		if err := tmp.Close(); err != nil {
			os.Remove(tmp.Name())
			return err
		}
		if err := os.Chmod(tmp.Name(), 0o755); err != nil {
			os.Remove(tmp.Name())
			return err
		}
		return os.Rename(tmp.Name(), dest)
	}
}
