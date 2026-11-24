class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.9.10/altitude-0.9.10-aarch64-apple-darwin.tar.gz"
  sha256 "3dac427f4d474ddf300e63e89019351696ab3a1cd8a04bb5b5dbf58aeb283630"
  license "BUSL-1.1"
  version "0.9.10"

  depends_on :macos
  depends_on arch: :arm64

  def install
    # Homebrew stages a single-root archive inside that root, so the binary is either
    # here or one directory down; an archive that renames its root still installs.
    bin.install Dir["altitude", "*/altitude"].first
  end

  test do
    assert_match "altitude v#{version}", shell_output("#{bin}/altitude --version")
  end
end
