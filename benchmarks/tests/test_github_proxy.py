from __future__ import annotations

import unittest

from zg_bench.github_proxy import (
    GithubProxyMixin,
    normalize_github_proxy_prefix,
    rewrite_github_downloads,
)


class GithubProxyTests(unittest.TestCase):
    def test_rewrites_release_raw_and_clone_urls(self) -> None:
        command = (
            "curl -fL "
            "https://github.com/anomalyco/opencode/releases/download/v1/a.tar.gz; "
            "curl https://raw.githubusercontent.com/nvm-sh/nvm/main/install.sh; "
            "git clone https://github.com/example/repo.git"
        )

        rewritten = rewrite_github_downloads(
            command,
            "https://gh-proxy.com/",
        )

        self.assertIn(
            "https://gh-proxy.com/https://github.com/anomalyco/opencode",
            rewritten,
        )
        self.assertIn(
            "https://gh-proxy.com/https://raw.githubusercontent.com/nvm-sh/nvm",
            rewritten,
        )
        self.assertIn(
            "https://gh-proxy.com/https://github.com/example/repo.git",
            rewritten,
        )

    def test_rewrites_uv_installer_download_base(self) -> None:
        command = (
            "RUN curl -fsSL --retry 3 "
            "https://astral.sh/uv/0.7.13/install.sh | sh"
        )

        rewritten = rewrite_github_downloads(
            command,
            "https://gh-proxy.com/",
        )

        self.assertIn(
            "UV_INSTALLER_GITHUB_BASE_URL="
            "https://gh-proxy.com/https://github.com",
            rewritten,
        )
        self.assertIn(
            "curl -fsSL --retry 3 "
            "https://astral.sh/uv/0.7.13/install.sh",
            rewritten,
        )
        self.assertNotIn("&&  -fsSL", rewritten)
        self.assertIn("https://astral.sh/uv/0.7.13/install.sh", rewritten)

    def test_rewrites_nvm_installer_and_its_internal_git_clone(self) -> None:
        command = (
            "wget -qO- "
            "https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh "
            "| bash"
        )

        rewritten = rewrite_github_downloads(command, "https://gh-proxy.com/")

        self.assertIn(
            "NVM_SOURCE=https://gh-proxy.com/"
            "https://github.com/nvm-sh/nvm.git",
            rewritten,
        )
        self.assertIn(
            "https://gh-proxy.com/"
            "https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh",
            rewritten,
        )
        self.assertIn("wget -qO-", rewritten)

    def test_does_not_double_proxy_an_existing_url(self) -> None:
        command = (
            "curl https://gh-proxy.com/https://github.com/example/repo/archive.zip"
        )

        rewritten = rewrite_github_downloads(command, "https://gh-proxy.com/")

        self.assertEqual(rewritten, command)

    def test_requires_https_proxy_prefix(self) -> None:
        with self.assertRaisesRegex(ValueError, "HTTPS"):
            normalize_github_proxy_prefix("http://gh-proxy.com/")

    def test_rejects_shell_metacharacters_in_proxy_prefix(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsafe"):
            normalize_github_proxy_prefix(
                "https://gh-proxy.com/;touch/tmp/unexpected"
            )

    def test_agent_environment_covers_variable_based_installers(self) -> None:
        proxy = GithubProxyMixin(
            github_proxy_prefix="https://gh-proxy.com/"
        )

        environment = proxy._proxy_environment({"EXISTING": "value"})

        self.assertEqual(environment["EXISTING"], "value")
        self.assertEqual(
            environment["UV_INSTALLER_GITHUB_BASE_URL"],
            "https://gh-proxy.com/https://github.com",
        )
        self.assertEqual(
            environment["NVM_SOURCE"],
            "https://gh-proxy.com/https://github.com/nvm-sh/nvm.git",
        )


if __name__ == "__main__":
    unittest.main()
