class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.9.2/altitude-0.9.2-aarch64-apple-darwin.tar.gz"
  sha256 "d07cff9d7a3038fa4297b744bf9a5ea3cc075c21fa3c8168360ebab827be9703"
  license "BUSL-1.1"
  version "0.9.2"

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
