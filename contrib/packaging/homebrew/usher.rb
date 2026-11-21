class Usher < Formula
  desc "Zero-daemon, SSH-bootstrapped remote desktop client and host"
  homepage "https://github.com/rdenno/usher"
  version "0.5.0"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/rdenno/usher/releases/download/v0.5.0/usher-macos-aarch64.tar.gz"
      sha256 "REPLACE_WITH_MACOS_AARCH64_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/rdenno/usher/releases/download/v0.5.0/usher-linux-aarch64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_AARCH64_SHA256"
    end
    on_intel do
      url "https://github.com/rdenno/usher/releases/download/v0.5.0/usher-linux-x86_64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X86_64_SHA256"
    end
  end

  def install
    bin.install "usher"
    bin.install "usher-host" if File.exist?("usher-host")
    pkgshare.install "99-usher-uinput.rules" if File.exist?("99-usher-uinput.rules")
    pkgshare.install "install-host.sh" if File.exist?("install-host.sh")
  end

  test do
    assert_match "usher #{version}", shell_output("#{bin}/usher --version")
  end
end
