import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import deploy as d  # noqa: E402

BASE = {"SHADW_REPO_URL": "https://github.com/acme/app", "SHADW_BRANCH": "main",
        "SHADW_COMMIT": "a" * 40, "SHADW_EVENT": "push"}


class ShadwYml(unittest.TestCase):
    def test_example_file_validates_with_minimal_parser(self):
        path = os.path.join(os.path.dirname(__file__), "..", "..", "examples", "shadw.yml")
        with open(path) as fh:
            cfg = d.validate(d.parse_minimal_yaml(fh.read()))
        self.assertEqual(cfg["version"], 1)

    def test_defaults_without_file(self):
        body, opts = d.build_request(d.load_config("/nonexistent/shadw.yml"), dict(BASE))
        self.assertEqual(body, {"repo_url": BASE["SHADW_REPO_URL"], "creator": "github-actions",
                                "branch": "main", "commit": "a" * 40})
        self.assertTrue(opts["wait"])

    def test_full_mapping(self):
        cfg = d.validate(d.parse_minimal_yaml(
            "version: 1\nproject: my-app\nroot_dir: apps/web\ntarget: production\n"
            "build:\n  cache: false\nenv:\n  NODE_ENV: production\ndeploy:\n  wait: false\n"))
        env = dict(BASE, SHADW_EXTRA_ENV="API_KEY=s3cret=x\n")
        body, opts = d.build_request(cfg, env)
        self.assertEqual(body["project"], "my-app")
        self.assertEqual(body["root_dir"], "apps/web")
        self.assertEqual(body["target"], "production")
        self.assertFalse(body["use_cache"])
        self.assertEqual(body["env"], {"NODE_ENV": "production", "API_KEY": "s3cret=x"})
        self.assertFalse(opts["wait"])

    def test_pull_request_is_preview(self):
        body, _ = d.build_request({}, dict(BASE, SHADW_EVENT="pull_request"))
        self.assertEqual(body["target"], "preview")

    def test_rejects_bad_config(self):
        for text in ("version: 2\n", "nope: 1\n", "project: Bad_Name\n",
                     "root_dir: ../x\n", "target: prod\n", "build:\n  foo: 1\n"):
            with self.assertRaises(d.ConfigError, msg=text):
                d.validate(d.parse_minimal_yaml(text))

    def test_load_from_file(self):
        with tempfile.NamedTemporaryFile("w", suffix=".yml", delete=False) as fh:
            fh.write("version: 1  # comment\nproject: 'demo'\n")
        try:
            self.assertEqual(d.load_config(fh.name)["project"], "demo")
        finally:
            os.unlink(fh.name)


if __name__ == "__main__":
    unittest.main()
