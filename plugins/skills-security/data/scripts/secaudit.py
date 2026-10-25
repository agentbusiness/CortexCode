#!/usr/bin/env python3
"""secaudit: the mechanical half of a source-code security audit, for CortexCode's model.

    python3 secaudit.py next                       the one driving command: maps the project on the first call, then the next lines to judge, then the report
    python3 secaudit.py settle <id>... safe|hardening|lead|finding|blocked ["note"]
    python3 secaudit.py recon [--scope <path>...] [--out <dir>]
    python3 secaudit.py hunt <unit>
    python3 secaudit.py cover <unit> covered|candidate|blocked ["<note>"]
    python3 secaudit.py new confirmed|needs_validation|rejected
    python3 secaudit.py finding <file.json>
    python3 secaudit.py status
    python3 secaudit.py report [--partial]
    python3 secaudit.py scan [<path>...] [--changed [<ref>]]
    python3 secaudit.py deps
    python3 secaudit.py cve <GHSA-... | CVE-...>

Runs from the repository root (every CortexCode `run` starts there). Writes only under
`<out>` (default `tmp/secaudit`, which git ignores). Python 3.8+, standard library only.
The attack-class catalog is `classes-*.json` beside this file: which classes apply to a
project is decided from structural facts (languages, imports, files, code shapes), and each
class carries the invariant, where to look, what counts as a finding, and the code shapes
to search for. The model does the tracing and the judging; this tool does the rest.

Live sources, queried at call time, never mirrored (each answers within seconds or is skipped
with one line when the project is offline):
- OSV (api.osv.dev): known vulnerabilities for the exact dependency versions in the project's
  lockfiles (npm, PyPI, crates.io, Go, RubyGems, Packagist, Maven, NuGet), with the fixed
  versions. OSV carries the GitHub Advisory Database, the ecosystems' own advisories and the CVE
  list. `recon` and `deps` use it; the report lists what it found.
- NVD (services.nvd.nist.gov): a CVE's description, CVSS and CWE ids when OSV has no record.
- The CWE API (cwe-api.mitre.org): the mitigations of a weakness class, for `cve`.
- The Cloudflare security-audit skill (raw.githubusercontent.com/cloudflare/security-audit-skill):
  the current hunting guidance for each attack class, shown by `hunt`.
The code shapes searched for (`classes-*.json` beside this file) are this tool's own heuristics,
written from that upstream guidance; a hit is a question to trace, never a finding.
"""

import fnmatch
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT_DEFAULT = "tmp/secaudit"
MAX_FILE_BYTES = 1_000_000
HITS_SHOWN = 40
HITS_PER_PATTERN = 12
HISTORY_COMMITS = 200
OSV_URL = "https://api.osv.dev/v1/"
NVD_URL = "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId="
CWE_URL = "https://cwe-api.mitre.org/api/v1/cwe/weakness/"
UPSTREAM_URL = "https://raw.githubusercontent.com/cloudflare/security-audit-skill/main/skills/security-audit/"
# Where each class's current guidance lives upstream: (file, the bold block in it, or None for a
# companion file whose core discipline and class list are shown).
UPSTREAM = {
    "obvious": ("ATTACK-CLASSES.md", "Obvious things"), "injection": ("ATTACK-CLASSES.md", "Injection"), "access-control": ("ATTACK-CLASSES.md", "Access control"),
    "files-resources": ("ATTACK-CLASSES.md", "Resource and file handling"), "crypto-secrets": ("ATTACK-CLASSES.md", "Cryptography and secrets"),
    "business-logic": ("ATTACK-CLASSES.md", "Business logic"), "data-exposure": ("ATTACK-CLASSES.md", "Feature abuse and data leakage"),
    "web-auth": ("WEB-PROTOCOL-AND-AUTH.md", None), "client-side": ("CLIENT-SIDE.md", None), "supply-chain-ci": ("SUPPLY-CHAIN-AND-RELEASE.md", None),
    "cloud-deploy": ("CLOUD-AND-DEPLOYMENT.md", None), "ai-llm-agent": ("AI-AND-LLM.md", None), "protocols-messaging": ("PROTOCOLS-RPC-AND-MESSAGING.md", None),
    "memory-safety": ("MEMORY-SAFETY-AND-BINARY.md", None), "desktop-ipc": ("DESKTOP-MOBILE-AND-LOCAL-IPC.md", None), "availability": ("RESOURCE-EXHAUSTION-AND-AVAILABILITY.md", None),
    "data-lifecycle": ("DATA-ISOLATION-AND-LIFECYCLE.md", None),
}
FETCH_TIMEOUT = 8
FETCH_MAX_BYTES = 4_000_000
OSV_BATCH = 1000
OSV_DETAIL_PACKAGES = 60
DEPS_SHOWN = 40
RECON_DEPS_SHOWN = 8

SKIP_DIRS = {".git", "node_modules", "target", "dist", "build", "out", ".next", ".nuxt", ".svelte-kit", "vendor", "venv", ".venv", "env", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox", ".idea", ".vscode", "coverage", "tmp", ".cache", "Pods", "DerivedData", ".gradle", ".terraform", "bower_components", ".yarn", ".pnpm-store", "site-packages", ".dart_tool", "bin", "obj"}
SKIP_FILE_GLOBS = ["*.min.js", "*.min.css", "*.map", "*.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "Cargo.lock", "go.sum", "poetry.lock", "Pipfile.lock", "composer.lock", "Gemfile.lock", "*.snap", "*.svg", "*.csv", "*.tsv", "*.jsonl", "*.ndjson", "*.pb.go", "*_pb2.py", "*.d.ts", "*.generated.*", "*.gen.go"]
LOCKFILES = ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "Cargo.lock", "go.sum", "poetry.lock", "Pipfile.lock", "uv.lock", "pdm.lock", "requirements.txt", "composer.lock", "Gemfile.lock", "packages.lock.json", "gradle.lockfile", "flake.lock"]
MANIFESTS = {"package.json": "npm", "requirements.txt": "pip", "requirements-dev.txt": "pip", "pyproject.toml": "python", "setup.py": "python", "Pipfile": "pipenv", "Cargo.toml": "cargo", "go.mod": "go", "Gemfile": "bundler", "composer.json": "composer", "pom.xml": "maven", "build.gradle": "gradle", "build.gradle.kts": "gradle", "*.csproj": "nuget", "Package.swift": "swiftpm", "pubspec.yaml": "pub", "mix.exs": "mix"}
MANIFEST_LOCK = {"package.json": ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb"], "pyproject.toml": ["poetry.lock", "uv.lock", "pdm.lock", "requirements.txt"], "Pipfile": ["Pipfile.lock"], "Cargo.toml": ["Cargo.lock"], "go.mod": ["go.sum"], "Gemfile": ["Gemfile.lock"], "composer.json": ["composer.lock"], "build.gradle": ["gradle.lockfile"], "build.gradle.kts": ["gradle.lockfile"]}
ENTRY_FILES = ["main.py", "app.py", "wsgi.py", "asgi.py", "manage.py", "server.py", "server.js", "server.ts", "index.js", "index.ts", "app.js", "app.ts", "main.js", "main.ts", "main.go", "main.rs", "Program.cs", "Application.java", "config.ru", "artisan", "index.php", "lib.rs", "mod.rs"]

LANG_BY_EXT = {
    ".py": "py", ".pyi": "py",
    ".js": "js", ".jsx": "js", ".ts": "js", ".tsx": "js", ".mjs": "js", ".cjs": "js", ".mts": "js", ".cts": "js", ".vue": "js", ".svelte": "js", ".astro": "js",
    ".go": "go", ".rs": "rs", ".java": "java", ".kt": "java", ".kts": "java", ".scala": "java", ".groovy": "java",
    ".rb": "rb", ".erb": "rb", ".php": "php", ".phtml": "php",
    ".c": "c", ".cc": "c", ".cpp": "c", ".cxx": "c", ".h": "c", ".hpp": "c", ".hh": "c", ".m": "c", ".mm": "c",
    ".cs": "cs", ".swift": "swift", ".sh": "sh", ".bash": "sh", ".zsh": "sh",
    ".html": "html", ".htm": "html", ".ejs": "html", ".hbs": "html", ".jinja": "html", ".jinja2": "html", ".j2": "html", ".njk": "html", ".tmpl": "html", ".twig": "html",
    ".yml": "yml", ".yaml": "yml", ".json": "json", ".toml": "toml", ".tf": "tf", ".tfvars": "tf", ".hcl": "tf", ".ini": "toml", ".cfg": "toml", ".env": "toml", ".properties": "toml", ".xml": "html", ".plist": "html", ".gradle": "java",
}
LANG_NAMES = {"py": "Python", "js": "JavaScript/TypeScript", "go": "Go", "rs": "Rust", "java": "Java/Kotlin", "rb": "Ruby", "php": "PHP", "c": "C/C++/Objective-C", "cs": "C#", "swift": "Swift", "sh": "shell", "html": "HTML/templates", "yml": "YAML", "json": "JSON", "toml": "TOML/config", "tf": "Terraform/HCL", "docker": "Dockerfile"}
CODE_LANGS = {"py", "js", "go", "rs", "java", "rb", "php", "c", "cs", "swift", "sh"}

# Frameworks and libraries worth naming in the recon facts: (shown name, import tokens).
STACK = [
    ("Flask", ["flask"]), ("Django", ["django"]), ("FastAPI", ["fastapi"]), ("Starlette", ["starlette"]), ("Tornado", ["tornado"]), ("aiohttp", ["aiohttp"]),
    ("Express", ["express"]), ("Koa", ["koa"]), ("Fastify", ["fastify"]), ("Hono", ["hono"]), ("Next.js", ["next"]), ("NestJS", ["@nestjs/core", "@nestjs/common"]), ("Nuxt", ["nuxt"]), ("Remix", ["@remix-run/node"]), ("SvelteKit", ["@sveltejs/kit"]),
    ("Gin", ["gin-gonic"]), ("Echo", ["labstack/echo"]), ("chi", ["go-chi/chi"]), ("Fiber", ["gofiber/fiber"]), ("net/http", ["net/http"]), ("gorilla/mux", ["gorilla/mux"]),
    ("actix-web", ["actix_web", "actix-web"]), ("axum", ["axum"]), ("Rocket", ["rocket"]), ("warp", ["warp"]), ("hyper", ["hyper"]), ("tonic (gRPC)", ["tonic"]),
    ("Spring", ["org.springframework", "spring"]), ("Jakarta/JAX-RS", ["jakarta.ws", "javax.ws"]), ("Rails", ["rails", "actionpack"]), ("Sinatra", ["sinatra"]), ("Laravel", ["laravel", "illuminate"]), ("Symfony", ["symfony"]), ("ASP.NET Core", ["Microsoft.AspNetCore"]),
    ("React", ["react", "react-dom"]), ("Vue", ["vue"]), ("Svelte", ["svelte"]), ("Angular", ["@angular/core"]), ("jQuery", ["jquery"]), ("Electron", ["electron"]), ("Tauri", ["@tauri-apps/api", "tauri"]), ("React Native", ["react-native"]),
    ("SQLAlchemy", ["sqlalchemy", "sqlmodel"]), ("Django ORM", ["django.db"]), ("psycopg", ["psycopg", "psycopg2"]), ("sqlite3", ["sqlite3"]), ("Prisma", ["@prisma/client", "prisma"]), ("Sequelize", ["sequelize"]), ("TypeORM", ["typeorm"]), ("Knex", ["knex"]), ("Drizzle", ["drizzle-orm"]), ("Mongoose", ["mongoose"]), ("pg", ["pg"]), ("mysql2", ["mysql2"]), ("GORM", ["gorm.io"]), ("sqlx", ["sqlx", "jmoiron/sqlx"]), ("diesel", ["diesel"]), ("sea-orm", ["sea_orm", "sea-orm"]), ("JDBC/JPA", ["java.sql", "jakarta.persistence", "javax.persistence", "org.hibernate"]), ("ActiveRecord", ["active_record", "activerecord"]),
    ("Redis", ["redis", "ioredis", "go-redis"]), ("Kafka", ["kafka", "kafkajs", "confluent_kafka", "segmentio/kafka-go"]), ("RabbitMQ", ["pika", "amqplib", "aio_pika", "amqp"]), ("Celery", ["celery"]), ("BullMQ", ["bullmq", "bull"]), ("NATS", ["nats"]),
    ("GraphQL", ["graphql", "@apollo/server", "apollo-server", "strawberry", "graphene", "ariadne", "gqlgen", "async-graphql"]), ("gRPC", ["grpc", "@grpc/grpc-js", "google.golang.org/grpc"]), ("WebSocket", ["ws", "websockets", "socket.io", "gorilla/websocket", "tokio-tungstenite"]),
    ("JWT", ["jwt", "jsonwebtoken", "jose", "pyjwt", "python-jose", "golang-jwt", "jsonwebtoken (rust)", "io.jsonwebtoken"]), ("Passport", ["passport"]), ("Flask-Login", ["flask_login"]), ("Django auth", ["django.contrib.auth"]), ("NextAuth/Auth.js", ["next-auth", "@auth/core"]), ("OAuth client", ["authlib", "oauthlib", "requests_oauthlib", "golang.org/x/oauth2", "oauth2", "passport-oauth2"]), ("bcrypt/argon2/scrypt", ["bcrypt", "argon2", "scrypt", "passlib", "golang.org/x/crypto/bcrypt"]), ("CSRF protection", ["flask_wtf", "csurf", "csrf", "@fastify/csrf-protection", "django.middleware.csrf"]), ("helmet", ["helmet"]), ("CORS middleware", ["flask_cors", "cors", "@fastify/cors", "rs/cors", "tower_http::cors", "corsheaders"]), ("rate limiting", ["flask_limiter", "slowapi", "express-rate-limit", "rate-limiter-flexible", "@fastify/rate-limit", "ulule/limiter", "tower_governor", "rack-attack"]),
    ("cryptography", ["cryptography", "Crypto", "pycryptodome", "nacl", "ring", "rustls", "openssl", "crypto/aes", "crypto/rsa", "javax.crypto", "System.Security.Cryptography"]),
    ("OpenAI SDK", ["openai"]), ("Anthropic SDK", ["anthropic", "@anthropic-ai/sdk"]), ("LangChain", ["langchain", "@langchain/core", "langchain_core", "langchain_openai"]), ("LlamaIndex", ["llama_index", "llamaindex"]), ("Vercel AI SDK", ["ai", "@ai-sdk/openai", "@ai-sdk/anthropic"]), ("MCP", ["mcp", "@modelcontextprotocol/sdk"]), ("Ollama", ["ollama"]), ("LiteLLM", ["litellm"]),
    ("AWS SDK", ["boto3", "botocore", "aws-sdk", "@aws-sdk/client-s3", "@aws-sdk", "github.com/aws/aws-sdk-go", "aws-sdk-s3", "software.amazon.awssdk", "com.amazonaws"]), ("Google Cloud SDK", ["google.cloud", "@google-cloud", "cloud.google.com/go"]), ("Azure SDK", ["azure", "@azure", "Azure."]), ("Stripe", ["stripe"]), ("Twilio", ["twilio"]), ("SendGrid", ["sendgrid"]),
    ("Docker SDK", ["docker", "dockerode"]), ("Kubernetes client", ["kubernetes", "@kubernetes/client-node", "k8s.io/client-go"]),
    ("subprocess/child_process", ["subprocess", "child_process", "os/exec", "std::process"]), ("pickle", ["pickle", "cPickle", "dill"]), ("PyYAML", ["yaml"]), ("requests/httpx", ["requests", "httpx", "urllib", "urllib3"]), ("axios/node-fetch", ["axios", "node-fetch", "got", "undici"]), ("reqwest", ["reqwest"]),
    ("Terraform/Pulumi/CDK", ["pulumi", "@pulumi/pulumi", "aws-cdk-lib", "constructs"]), ("Serverless/Edge runtime", ["@cloudflare/workers-types", "wrangler", "@vercel/node", "aws-lambda", "@types/aws-lambda", "serverless"]),
]

IMPORT_RES = {
    "py": [re.compile(r"^\s*(?:from\s+([\w.]+)\s+import|import\s+([\w.]+))", re.M)],
    "js": [re.compile(r"""(?:\bfrom\s+|\brequire\s*\(\s*|\bimport\s*\(\s*|^\s*import\s+)['"]([^'"\n]+)['"]""", re.M)],
    "go": [re.compile(r'^\s*(?:import\s+)?(?:[\w.]+\s+)?"([\w./@-]+)"\s*$', re.M)],
    "rs": [re.compile(r"^\s*(?:pub\s+)?use\s+([a-z_][\w]*)", re.M), re.compile(r"^\s*extern\s+crate\s+(\w+)", re.M)],
    "java": [re.compile(r"^\s*import\s+(?:static\s+)?([\w.]+)", re.M)],
    "rb": [re.compile(r"""^\s*require(?:_relative)?\s+['"]([^'"]+)['"]""", re.M), re.compile(r"""^\s*gem\s+['"]([^'"]+)['"]""", re.M)],
    "php": [re.compile(r"^\s*use\s+([\w\\]+)", re.M)],
    "cs": [re.compile(r"^\s*using\s+([\w.]+)\s*;", re.M)],
    "swift": [re.compile(r"^\s*import\s+(\w+)", re.M)],
}
MANIFEST_RES = {
    "package.json": re.compile(r'"([@\w./-]+)"\s*:\s*"[^"]*"'),
    "requirements": re.compile(r"^\s*([A-Za-z0-9][A-Za-z0-9_.-]*)", re.M),
    "pyproject.toml": re.compile(r'"([A-Za-z0-9][A-Za-z0-9_.-]*)\s*[\[>=<~!;"]'),
    "Cargo.toml": re.compile(r"^\s*([a-zA-Z0-9_-]+)\s*=\s*(?:\"[^\"]*\"|\{)", re.M),
    "go.mod": re.compile(r"^\s*([\w./-]+)\s+v[\w.+-]+", re.M),
    "Gemfile": re.compile(r"""^\s*gem\s+['"]([^'"]+)['"]""", re.M),
    "composer.json": re.compile(r'"([\w./-]+)"\s*:\s*"[^"]*"'),
    "pom.xml": re.compile(r"<(?:groupId|artifactId)>([^<]+)</(?:groupId|artifactId)>"),
    "build.gradle": re.compile(r"""['"]([\w.-]+):([\w.-]+):[^'"]*['"]"""),
}
SECRET_FILE_NAMES = [".env", "*.pem", "*.key", "*.p12", "*.pfx", "*.jks", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "credentials.json", "*.tfstate", ".npmrc", ".pypirc", ".netrc"]
SEVERITY = ["informational", "low", "medium", "high", "critical"]
STATUSES = ["planned", "in_progress", "covered", "candidate", "blocked"]
VERDICTS = ["confirmed", "needs_validation", "rejected"]


# ----------------------------------------------------------------------------- files

def lang_of(path):
    name = os.path.basename(path)
    if name == "Dockerfile" or name.startswith("Dockerfile.") or name.endswith(".dockerfile"):
        return "docker"
    if name in ("Makefile", "Jenkinsfile", "Procfile", "Vagrantfile", "Rakefile", "Gemfile", "Podfile"):
        return "sh" if name in ("Makefile", "Jenkinsfile", "Procfile") else "rb"
    if name.startswith(".env"):
        return "toml"
    return LANG_BY_EXT.get(os.path.splitext(name)[1].lower())


def skipped(rel):
    parts = rel.split("/")
    if any(part in SKIP_DIRS for part in parts[:-1]):
        return True
    name = parts[-1]
    return any(fnmatch.fnmatch(name, glob) for glob in SKIP_FILE_GLOBS)


def git(args, root=".", limit=None):
    try:
        out = subprocess.run(["git"] + args, cwd=root, capture_output=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if out.returncode != 0:
        return None
    data = out.stdout if limit is None else out.stdout[:limit]
    return data.decode("utf-8", "replace")


def repo_files(root, scope):
    """Repository-relative paths worth scanning: git's tracked and untracked-unignored files when
    the root is a repository, else a walk; without ignored, generated, huge or binary files."""
    listed = git(["ls-files", "-co", "--exclude-standard", "-z"], root)
    if listed is not None:
        rels = [rel for rel in listed.split("\0") if rel]
    else:
        rels = []
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")]
            for name in filenames:
                rels.append(os.path.relpath(os.path.join(dirpath, name), root))
    scope = [s.rstrip("/") for s in scope]
    files = []
    repo_files.lockfiles = []
    for rel in sorted(set(rels)):
        if scope and not any(rel == s or rel.startswith(s + "/") for s in scope):
            continue
        base = os.path.basename(rel)
        if (base in LOCKFILES or base in ("go.mod", "pom.xml") or (base.startswith("requirements") and base.endswith(".txt"))) and not any(part in SKIP_DIRS for part in rel.split("/")[:-1]):
            repo_files.lockfiles.append(rel)
        if skipped(rel):
            continue
        if scope and not any(rel == s or rel.startswith(s + "/") for s in scope):
            continue
        full = os.path.join(root, rel)
        try:
            if not os.path.isfile(full) or os.path.islink(full) or os.path.getsize(full) > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        files.append(rel)
    return files


def read_text(path):
    try:
        with open(path, "rb") as handle:
            data = handle.read()
    except OSError:
        return None
    if b"\0" in data[:8192]:
        return None
    return data.decode("utf-8", "replace")


def basename_matches(rel, globs):
    name = os.path.basename(rel)
    return any(fnmatch.fnmatch(name, g) or fnmatch.fnmatch(rel, g) or (("/" in g) and fnmatch.fnmatch(rel, "*/" + g)) for g in globs)


# ----------------------------------------------------------------------------- catalog

CLASS_KEYS = ("id", "name", "applies", "invariant", "look", "counts", "hardening", "patterns")


def load_catalog(directory):
    """The classes of every `classes-*.json` in `directory`, checked and with their regexes
    compiled; raises ValueError naming the first problem."""
    paths = sorted(Path(directory).glob("classes-*.json"))
    if not paths:
        raise ValueError("no classes-*.json in %s" % directory)
    classes, ids = [], set()
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        if not isinstance(data, dict) or not isinstance(data.get("classes"), list):
            raise ValueError("%s: expected {\"classes\": [...]}" % path.name)
        for cls in data["classes"]:
            for key in CLASS_KEYS:
                if key not in cls:
                    raise ValueError("%s: class %s lacks %s" % (path.name, cls.get("id", "?"), key))
            if not re.match(r"^[a-z][a-z0-9-]*$", cls["id"]) or cls["id"] in ids:
                raise ValueError("%s: class id %r is invalid or repeated" % (path.name, cls["id"]))
            ids.add(cls["id"])
            for pattern in cls["patterns"]:
                for key in ("name", "langs", "needle", "regex"):
                    if key not in pattern:
                        raise ValueError("%s: a pattern of %s lacks %s" % (path.name, cls["id"], key))
                if len(pattern["regex"]) > 800:
                    raise ValueError("%s: a regex of %s is longer than 800 characters" % (path.name, cls["id"]))
                pattern["re"] = re.compile(pattern["regex"])
            cls["content_re"] = [re.compile(p) for p in cls["applies"].get("content", [])]
            classes.append(cls)
    return classes


PREVIOUS_CLASS = {
    "id": "previous", "name": "Issues of the previous audit", "applies": {}, "patterns": [],
    "invariant": "Every issue the previous audit reported is either fixed in the current code or still open and carried into this report.",
    "look": [
        "A confirmed finding or lead: read the flagged line and its callers again; the pre-check says whether the line is unchanged, moved or gone, which is a hint, not a verdict.",
        "A hardening note: is the change made (an upgrade, a moved secret, a tightened check)?",
        "A dependency note: the pre-check compares the pinned version with the previous one and OSV's current answer."
    ],
    "counts": "A previous finding whose path still exists unchanged is still a finding: settle it as still. A previous issue that the code no longer has is fixed.",
    "hardening": "A previous hardening note not acted on stays a hardening note (still).",
}


class Catalog:
    def __init__(self, directory=None):
        self.directory = directory or str(HERE)
        self.classes = load_catalog(self.directory)
        self.by_id = {cls["id"]: cls for cls in self.classes}
        self.by_id["previous"] = PREVIOUS_CLASS

    def patterns_for(self, lang):
        """(class id, pattern) pairs that apply to files of `lang`."""
        out = []
        for cls in self.classes:
            for pattern in cls.get("patterns", []):
                langs = pattern["langs"]
                if "any" in langs or (lang and lang in langs):
                    out.append((cls["id"], pattern))
        return out


def fetch(url, body=None):
    """The bytes at `url` (POST of `body` as JSON when given), within the timeout and size cap;
    raises OSError or ValueError. HTTPS only (the loopback host is allowed for local checks)."""
    if not url.startswith("https://") and not url.startswith("http://127.0.0.1"):
        raise ValueError("live sources are read over https only")
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {"User-Agent": "secaudit (CortexCode)", "Accept": "application/json, text/plain"}
    if data is not None:
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT) as response:
        payload = response.read(FETCH_MAX_BYTES + 1)
    if len(payload) > FETCH_MAX_BYTES:
        raise ValueError("%s is larger than %d bytes" % (url, FETCH_MAX_BYTES))
    return payload


def fetch_json(url, body=None):
    return json.loads(fetch(url, body).decode("utf-8"))


def offline_here(root):
    return os.path.exists(os.path.join(root, ".cortex-offline"))


LIVE_ERRORS = (OSError, ValueError, KeyError, TypeError, IndexError, urllib.error.URLError)


# ----------------------------------------------------------------------------- lockfiles

def parse_lockfile(rel, text):
    """(ecosystem, name, version) for every package a lockfile pins; [] for a file that is not one."""
    name = os.path.basename(rel)
    out = []
    try:
        if name == "package-lock.json":
            data = json.loads(text)
            for path, info in (data.get("packages") or {}).items():
                if path and isinstance(info, dict) and info.get("version") and "node_modules/" in path:
                    out.append(("npm", path.rsplit("node_modules/", 1)[1], info["version"]))
            def walk(deps):
                for pkg, info in (deps or {}).items():
                    if isinstance(info, dict) and info.get("version"):
                        out.append(("npm", pkg, info["version"]))
                        walk(info.get("dependencies"))
            if not data.get("packages"):
                walk(data.get("dependencies"))
        elif name == "yarn.lock":
            for match in re.finditer(r'^"?((?:@[^/\s"]+/)?[^@\s"]+)@[^\n]*:\n(?:[^\n]*\n)*?\s+version:?\s+"?([^"\s]+)"?', text, re.M):
                out.append(("npm", match.group(1), match.group(2)))
        elif name == "pnpm-lock.yaml":
            section = text.split("\npackages:", 1)
            if len(section) == 2:
                for match in re.finditer(r"^  '?/?((?:@[^/\s']+/)?[^@\s'/]+)@([^\s'(:]+)", section[1], re.M):
                    out.append(("npm", match.group(1), match.group(2)))
        elif name.startswith("requirements") and name.endswith(".txt"):
            for match in re.finditer(r"^\s*([A-Za-z0-9][A-Za-z0-9_.-]*)\s*(?:\[[^\]]*\])?\s*==\s*([A-Za-z0-9_.+!-]+)", text, re.M):
                out.append(("PyPI", match.group(1), match.group(2)))
        elif name in ("poetry.lock", "Cargo.lock", "pdm.lock", "uv.lock"):
            eco = "crates.io" if name == "Cargo.lock" else "PyPI"
            for match in re.finditer(r'\[\[package\]\]\s*\nname\s*=\s*"([^"]+)"\s*\nversion\s*=\s*"([^"]+)"', text):
                out.append((eco, match.group(1), match.group(2)))
        elif name == "Pipfile.lock":
            data = json.loads(text)
            for section in ("default", "develop"):
                for pkg, info in (data.get(section) or {}).items():
                    version = (info or {}).get("version", "")
                    if version.startswith("=="):
                        out.append(("PyPI", pkg, version[2:]))
        elif name == "go.mod":
            body = re.sub(r"//[^\n]*", "", text)
            for match in re.finditer(r"^\s*(?:require\s+)?([\w.~-]+(?:/[\w.~-]+)+)\s+(v[\w.+-]+)", body, re.M):
                out.append(("Go", match.group(1), match.group(2)))
        elif name == "Gemfile.lock":
            for match in re.finditer(r"^    ([A-Za-z0-9_.-]+) \(([^)\s]+)\)\s*$", text, re.M):
                out.append(("RubyGems", match.group(1), match.group(2)))
        elif name == "composer.lock":
            data = json.loads(text)
            for pkg in (data.get("packages") or []) + (data.get("packages-dev") or []):
                if pkg.get("name") and pkg.get("version"):
                    out.append(("Packagist", pkg["name"], pkg["version"].lstrip("v")))
        elif name == "packages.lock.json":
            data = json.loads(text)
            for frameworks in (data.get("dependencies") or {}).values():
                for pkg, info in (frameworks or {}).items():
                    if isinstance(info, dict) and info.get("resolved"):
                        out.append(("NuGet", pkg, info["resolved"]))
        elif name == "pom.xml":
            for block in re.findall(r"<dependency>(.*?)</dependency>", text, re.S):
                group = re.search(r"<groupId>\s*([^<\s]+)\s*</groupId>", block)
                artifact = re.search(r"<artifactId>\s*([^<\s]+)\s*</artifactId>", block)
                version = re.search(r"<version>\s*([^<\s$]+)\s*</version>", block)
                if group and artifact and version:
                    out.append(("Maven", group.group(1) + ":" + artifact.group(1), version.group(1)))
    except (ValueError, AttributeError):
        return []
    seen, unique = set(), []
    for item in out:
        if item not in seen:
            seen.add(item)
            unique.append(item)
    return unique


def project_packages(root, files):
    """Every pinned package of the project's lockfiles: {(ecosystem, name, version): lockfile}."""
    packages = {}
    for rel in sorted(set(getattr(repo_files, "lockfiles", []))):
        text = read_text(os.path.join(root, rel))
        if text is None:
            continue
        for item in parse_lockfile(rel, text):
            packages.setdefault(item, rel)
    return packages


# ----------------------------------------------------------------------------- live: OSV, NVD, CWE, upstream

CVSS_WEIGHTS = {"AV": {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2}, "AC": {"L": 0.77, "H": 0.44}, "UI": {"N": 0.85, "R": 0.62}, "C": {"H": 0.56, "L": 0.22, "N": 0.0}, "I": {"H": 0.56, "L": 0.22, "N": 0.0}, "A": {"H": 0.56, "L": 0.22, "N": 0.0}}


def cvss3_rating(vector):
    """The CVSS 3.x base score of a vector string as a word; None when the vector is not one."""
    parts = dict(item.split(":", 1) for item in vector.split("/")[1:] if ":" in item)
    try:
        scope_changed = parts["S"] == "C"
        privileges = {"N": 0.85, "L": 0.68 if scope_changed else 0.62, "H": 0.5 if scope_changed else 0.27}[parts["PR"]]
        iss = 1 - (1 - CVSS_WEIGHTS["C"][parts["C"]]) * (1 - CVSS_WEIGHTS["I"][parts["I"]]) * (1 - CVSS_WEIGHTS["A"][parts["A"]])
        impact = 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02) ** 15 if scope_changed else 6.42 * iss
        exploitability = 8.22 * CVSS_WEIGHTS["AV"][parts["AV"]] * CVSS_WEIGHTS["AC"][parts["AC"]] * privileges * CVSS_WEIGHTS["UI"][parts["UI"]]
    except KeyError:
        return None
    if impact <= 0:
        return "none"
    score = min(1.08 * (impact + exploitability), 10) if scope_changed else min(impact + exploitability, 10)
    score = int(score * 10 + 0.999999) / 10
    return "critical" if score >= 9 else "high" if score >= 7 else "medium" if score >= 4 else "low" if score > 0 else "none"


def severity_of(vuln):
    specific = (vuln.get("database_specific") or {}).get("severity")
    if isinstance(specific, str) and specific:
        return {"moderate": "medium"}.get(specific.lower(), specific.lower())
    for entry in vuln.get("severity") or []:
        score = str(entry.get("score", ""))
        if score.startswith("CVSS:3"):
            rating = cvss3_rating(score)
            if rating:
                return rating
    return "unrated"


def fixed_versions(vuln, name):
    fixed = []
    for affected in vuln.get("affected") or []:
        if (affected.get("package") or {}).get("name", "").lower() != name.lower():
            continue
        for rng in affected.get("ranges") or []:
            if rng.get("type") == "GIT":
                continue
            for event in rng.get("events") or []:
                if event.get("fixed") and not re.fullmatch(r"[0-9a-f]{40}", event["fixed"]):
                    fixed.append(event["fixed"])
    return fixed


def osv_check(packages):
    """OSV's known vulnerabilities for `packages` ({(eco, name, version): lockfile}): a list of rows
    {eco, name, version, lockfile, id, aliases, severity, summary, fixed, cwes}, sorted worst first.
    Raises on network trouble."""
    items = list(packages.items())[:3 * OSV_BATCH]
    flagged = []
    for start in range(0, len(items), OSV_BATCH):
        chunk = items[start:start + OSV_BATCH]
        queries = [{"package": {"name": name, "ecosystem": eco}, "version": version} for (eco, name, version), _ in chunk]
        result = fetch_json(OSV_URL + "querybatch", {"queries": queries})
        for (key, lockfile), answer in zip(chunk, result.get("results") or []):
            if answer.get("vulns"):
                flagged.append((key, lockfile))
    rows = []
    for (eco, name, version), lockfile in flagged[:OSV_DETAIL_PACKAGES]:
        answer = fetch_json(OSV_URL + "query", {"package": {"name": name, "ecosystem": eco}, "version": version})
        for vuln in answer.get("vulns") or []:
            if vuln.get("withdrawn"):
                continue
            aliases = [a for a in vuln.get("aliases") or [] if a.startswith("CVE-")]
            summary = (vuln.get("summary") or (vuln.get("details") or "").split("\n")[0]).strip()
            rows.append({"eco": eco, "name": name, "version": version, "lockfile": lockfile, "id": vuln["id"], "aliases": aliases, "severity": severity_of(vuln), "summary": summary[:120], "fixed": fixed_versions(vuln, name), "cwes": (vuln.get("database_specific") or {}).get("cwe_ids") or []})
    rows.sort(key=lambda r: (SEVERITY_RANK.get(r["severity"], 5), 0 if r["id"].startswith("GHSA-") else 1, r["name"], r["id"]))
    unique, seen = [], set()
    for row in rows:
        keys = {(row["eco"], row["name"], row["version"], alias) for alias in [row["id"]] + row["aliases"]}
        if keys & seen:
            continue
        seen |= keys
        unique.append(row)
    return unique, len(flagged) - min(len(flagged), OSV_DETAIL_PACKAGES)


SEVERITY_RANK = {"critical": 0, "high": 1, "medium": 2, "low": 3, "none": 4, "unrated": 5}


def version_key(text):
    return tuple(int(part) if part.isdigit() else part for part in re.split(r"[.+-]", text))


def covering_fix(rows):
    """The one version to upgrade to: for each advisory the lowest fixed version on the installed
    major line (else its lowest fixed version), and the highest of those; None without any."""
    choices = []
    for row in rows:
        fixed = sorted(set(row["fixed"]), key=version_key)
        if not fixed:
            continue
        major = version_key(row["version"].lstrip("v"))[:1]
        same_line = [f for f in fixed if version_key(f.lstrip("v"))[:1] == major and "-" not in f]
        choices.append((same_line or fixed)[0])
    if not choices:
        return None
    try:
        return max(choices, key=lambda v: version_key(v.lstrip("v")))
    except TypeError:
        return sorted(choices)[-1]


def deps_lines(rows, packages, more, shown=DEPS_SHOWN):
    """The dependency check as the model reads it."""
    if not packages:
        return ["No lockfile pins a dependency here (nothing for OSV to check)."]
    ecos = {}
    for (eco, _, _) in packages:
        ecos[eco] = ecos.get(eco, 0) + 1
    files = sorted(set(packages.values()))
    lines = ["%d pinned packages from %s (%s)." % (len(packages), ", ".join(files[:6]) + (" ..." if len(files) > 6 else ""), ", ".join("%s %d" % kv for kv in sorted(ecos.items())))]
    if not rows:
        lines.append("OSV knows no vulnerability for these exact versions (checked live).")
        return lines
    groups = {}
    for row in rows:
        groups.setdefault((row["eco"], row["name"], row["version"], row["lockfile"]), []).append(row)
    ordered = sorted(groups.items(), key=lambda kv: (min(SEVERITY_RANK.get(r["severity"], 5) for r in kv[1]), kv[0][1]))
    lines.append("Known vulnerabilities (OSV, live): %d advisories in %d packages%s." % (len(rows), len(groups), "; %d more packages flagged, listed in deps.json" % more if more else ""))
    for (eco, name, version, lockfile), group in ordered[:shown]:
        worst = group[0]["severity"]
        fix = covering_fix(group)
        lines.append("  %s %s %s (%s): %d advisor%s, worst %s; %s" % (eco, name, version, lockfile, len(group), "y" if len(group) == 1 else "ies", worst, ("upgrade to %s fixes every one listed" % fix) if fix else "no fixed version listed"))
        for row in group[:3]:
            ids = row["id"] + (" " + row["aliases"][0] if row["aliases"] else "")
            lines.append("    %s %s: %s" % (ids, row["severity"], row["summary"]))
        if len(group) > 3:
            lines.append("    ... %d more advisories in tmp/secaudit/deps.json" % (len(group) - 3))
    if len(ordered) > shown:
        lines.append("  ... %d more packages (all of them in tmp/secaudit/deps.json; the dependencies unit shows each as a line to judge)" % (len(ordered) - shown))
    return lines


def live_deps(state, root, files, shown=DEPS_SHOWN):
    """Run the OSV check, keep deps.json, return the lines to print (or [] when quiet and clean)."""
    packages = project_packages(root, files)
    if offline_here(root):
        return ["Offline project: the live dependency check (OSV) did not run; %d pinned packages unchecked." % len(packages)] if packages else []
    try:
        rows, more = osv_check(packages)
    except LIVE_ERRORS as error:
        return ["The live dependency check (OSV) could not run (%s); %d pinned packages unchecked." % (getattr(error, "reason", None) or error, len(packages))]
    state.save("deps.json", {"checked": time.strftime("%Y-%m-%d %H:%M"), "packages": len(packages), "rows": rows, "more": more})
    return deps_lines(rows, packages, more, shown)


def upstream_guidance(class_id):
    """The class's current guidance from the Cloudflare skill, as a few lines; [] when unreachable."""
    spec = UPSTREAM.get(class_id)
    if not spec:
        return []
    file_name, block = spec
    try:
        text = fetch(UPSTREAM_URL + file_name).decode("utf-8", "replace")
    except LIVE_ERRORS:
        return []
    lines = []
    if block:
        match = re.search(r"^\*\*" + re.escape(block) + r"\*\*[^\n]*\n(.*?)(?=^\*\*|^##|\Z)", text, re.M | re.S)
        if match:
            body = match.group(1).strip()
            for raw in body.split("\n"):
                raw = raw.rstrip()
                if raw and len("\n".join(lines)) < 1800:
                    lines.append(re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", raw)[:400])
    else:
        discipline = re.search(r"## Core discipline[^\n]*\n+```\n(.*?)```", text, re.S)
        if discipline:
            lines.extend(line.rstrip()[:400] for line in discipline.group(1).strip().split("\n") if line.strip())
        names = re.findall(r"^\*\*([^*\n]+)\*\*\s*$", text, re.M)
        if names:
            lines.append("Classes upstream: " + "; ".join(names[:20]))
    return lines


def cmd_deps(args):
    root = os.getcwd()
    state = State(root, default_out())
    files = repo_files(root, [])
    for line in live_deps(state, root, files):
        print(line)
    if os.path.isfile(state.path("deps.json")) and (state.load("deps.json", {}) or {}).get("rows"):
        print("Each vulnerable package: is the affected code called with outside data? (read graph://used-by/<function>, grep -rn \"<package>\" --include=*.*). Reachable: a confirmed finding with the upgrade as the fix; not reachable: a hardening note naming the upgrade.")
    print("Next: %s status" % tool())


def cmd_cve(args):
    if not args or not re.match(r"^(CVE-\d{4}-\d+|GHSA-[\w-]+|[A-Z]+-[\w:.-]+)$", args[0]):
        fail("usage: cve <CVE-YYYY-NNNN | GHSA-xxxx-xxxx-xxxx>", "%s cve CVE-2021-23337" % tool())
    ident = args[0]
    if offline_here(os.getcwd()):
        fail("offline project: no live lookup", "read the project's lockfiles for the affected package instead")
    vuln = None
    try:
        vuln = fetch_json(OSV_URL + "vulns/" + ident)
        advisory = next((a for a in vuln.get("aliases") or [] if a.startswith("GHSA-")), None)
        if ident.startswith("CVE-") and advisory:
            better = fetch_json(OSV_URL + "vulns/" + advisory)
            better["aliases"] = sorted(set((better.get("aliases") or []) + [ident]))
            vuln = better
    except LIVE_ERRORS:
        vuln = vuln or None
    cwes = []
    if vuln:
        print("%s%s: %s" % (vuln["id"], (" (" + ", ".join(vuln.get("aliases") or []) + ")") if vuln.get("aliases") else "", (vuln.get("summary") or (vuln.get("details") or "").split("\n")[0])[:200]))
        print("Severity: " + severity_of(vuln))
        for affected in (vuln.get("affected") or [])[:8]:
            pkg = affected.get("package") or {}
            if not pkg.get("name"):
                continue
            events = [e for rng in affected.get("ranges") or [] if rng.get("type") != "GIT" for e in rng.get("events") or []]
            introduced = [e["introduced"] for e in events if e.get("introduced")]
            fixed = [e["fixed"] for e in events if e.get("fixed")]
            print("Affects %s %s: from %s, fixed in %s" % (pkg.get("ecosystem", "?"), pkg.get("name", "?"), ", ".join(introduced) or "?", ", ".join(fixed) or "no fixed version"))
        cwes = (vuln.get("database_specific") or {}).get("cwe_ids") or []
        for ref in (vuln.get("references") or [])[:3]:
            print("Reference: " + ref.get("url", ""))
    elif ident.startswith("CVE-"):
        try:
            record = fetch_json(NVD_URL + ident)["vulnerabilities"][0]["cve"]
        except LIVE_ERRORS as error:
            fail("no record at OSV or NVD for %s (%s)" % (ident, getattr(error, "reason", None) or error), "check the id, or read the advisory the user gave")
        print("%s: %s" % (record["id"], next((d["value"] for d in record.get("descriptions", []) if d.get("lang") == "en"), "")[:300]))
        metrics = record.get("metrics") or {}
        for key in ("cvssMetricV31", "cvssMetricV30", "cvssMetricV2"):
            if metrics.get(key):
                data = metrics[key][0]["cvssData"]
                print("Severity: CVSS %s %s (%s)" % (data.get("version"), data.get("baseScore"), (metrics[key][0].get("baseSeverity") or data.get("baseSeverity") or "").lower()))
                break
        cwes = [d["value"] for w in record.get("weaknesses") or [] for d in w.get("description") or [] if d.get("value", "").startswith("CWE-")]
        for ref in (record.get("references") or [])[:3]:
            print("Reference: " + ref.get("url", ""))
    else:
        fail("no record at OSV for %s" % ident, "check the id")
    if cwes:
        print("Weakness: " + ", ".join(cwes[:3]))
        try:
            weakness = fetch_json(CWE_URL + cwes[0].split("-")[1])["Weaknesses"][0]
            mitigations = [m.get("Description", "") for m in weakness.get("PotentialMitigations") or [] if m.get("Description")]
            if mitigations:
                print("Mitigations (CWE, live):")
                for item in mitigations[:3]:
                    print("  - " + re.sub(r"\s+", " ", item)[:220])
        except LIVE_ERRORS:
            pass
    print("Next: find whether this project ships an affected version (grep -n \"<package>\" in the lockfiles) and whether the affected code is reached with outside data (read graph://used-by/<function>).")


def import_matches(entry, imports):
    e = entry.lower()
    for tok in imports:
        t = tok.lower()
        if t == e or t.startswith(e + "/") or t.startswith(e + ".") or t.endswith("/" + e) or ("/" + e + "/") in t:
            return tok
    return None


# ----------------------------------------------------------------------------- recon facts

def collect(root, files):
    """Read every file once: language counts, imports, manifests, lockfiles, entry points, texts."""
    facts = {"langs": {}, "imports": set(), "manifests": [], "lockfiles": [], "entries": [], "ci": [], "containers": [], "iac": [], "secret_files": [], "tests": False, "gitignore": None}
    texts = {}
    for rel in files:
        name = os.path.basename(rel)
        lang = lang_of(rel)
        text = read_text(os.path.join(root, rel))
        if text is None:
            continue
        texts[rel] = (lang, text)
        if lang:
            facts["langs"][lang] = facts["langs"].get(lang, 0) + 1
        for manifest, kind in MANIFESTS.items():
            if fnmatch.fnmatch(name, manifest):
                facts["manifests"].append((rel, kind))
                key = "requirements" if name.startswith("requirements") else ("build.gradle" if name.startswith("build.gradle") else name)
                regex = MANIFEST_RES.get(key)
                if regex:
                    for match in regex.finditer(text):
                        facts["imports"].update(g for g in match.groups() if g)
        if name in ENTRY_FILES or (lang == "go" and name == "main.go") or rel.startswith("cmd/"):
            facts["entries"].append(rel)
        if rel.startswith(".github/workflows/") or name in (".gitlab-ci.yml", "Jenkinsfile", "azure-pipelines.yml", ".travis.yml", "bitbucket-pipelines.yml") or rel.startswith(".circleci/"):
            facts["ci"].append(rel)
        if lang == "docker" or re.match(r"(docker-)?compose[\w.-]*\.ya?ml$", name):
            facts["containers"].append(rel)
        if lang == "tf" or name in ("serverless.yml", "wrangler.toml", "fly.toml", "Chart.yaml", "cdk.json", "Pulumi.yaml", "template.yaml", "cloudbuild.yaml") or re.search(r"(^|/)(k8s|kubernetes|manifests|deploy|helm)/", rel):
            facts["iac"].append(rel)
        if basename_matches(rel, SECRET_FILE_NAMES) and not name.endswith((".example", ".sample", ".template")):
            if name not in (".npmrc", ".pypirc", ".netrc") or re.search(r"(?i)(_authToken|password|token|secret)\s*[:=]", text):
                facts["secret_files"].append(rel)
        if re.search(r"(^|/)(tests?|spec|__tests__)(/|$)", rel) or re.search(r"(_test\.|\.test\.|\.spec\.|^test_)", name):
            facts["tests"] = True
        if rel == ".gitignore":
            facts["gitignore"] = text
        for regex in IMPORT_RES.get(lang, []):
            head = text[:20000]
            for match in regex.finditer(head):
                for group in match.groups():
                    if not group or group.startswith("."):
                        continue
                    tok = group
                    if lang == "js":
                        parts = tok.split("/")
                        tok = "/".join(parts[:2]) if tok.startswith("@") else parts[0]
                    if lang in ("py", "java", "cs", "php"):
                        segs = re.split(r"[.\\]", tok)
                        facts["imports"].add(segs[0])
                        if len(segs) > 1:
                            facts["imports"].add(".".join(segs[:2]))
                        if len(segs) > 2:
                            facts["imports"].add(".".join(segs[:3]))
                        continue
                    facts["imports"].add(tok)
    facts["imports"] = sorted(facts["imports"])
    facts["lockfiles"] = sorted(set(facts["lockfiles"]) | set(getattr(repo_files, "lockfiles", [])))
    return facts, texts


def stack_of(facts):
    found = []
    for name, tokens in STACK:
        for token in tokens:
            if import_matches(token, facts["imports"]):
                found.append(name)
                break
    return found


def applies_to(cls, facts, texts):
    """Whether the class applies here, and the structural fact that says so."""
    cond = cls.get("applies", {})
    if cond.get("always"):
        return True, "every project"
    for lang in cond.get("langs", []):
        if facts["langs"].get(lang):
            return True, "%s files" % LANG_NAMES.get(lang, lang)
    for entry in cond.get("imports", []):
        tok = import_matches(entry, facts["imports"])
        if tok:
            return True, "imports %s" % tok
    globs = cond.get("files", [])
    if globs:
        for rel in texts:
            if basename_matches(rel, globs):
                return True, rel
    for regex in cls.get("content_re", []):
        for rel, (lang, text) in texts.items():
            if lang not in CODE_LANGS:
                continue
            match = regex.search(text)
            if match:
                return True, "%s in %s" % (match.group(0).strip()[:40], rel)
    return False, ""


def scan_texts(catalog, texts, class_ids=None):
    """Pattern hits per class: {class id: [(rel, line number, pattern name, line text)]}."""
    hits = {}
    cache = {}
    for rel, (lang, text) in texts.items():
        key = lang or "-"
        if key not in cache:
            pairs = [(cid, p) for cid, p in catalog.patterns_for(lang) if class_ids is None or cid in class_ids]
            if lang is None:
                # Prose and unknown files: only secrets matter there.
                pairs = [(cid, p) for cid, p in pairs if cid == "obvious" and any(w in p["name"] for w in ("key", "token", "secret", "password", "connection", "JWT"))]
            cache[key] = pairs
        pairs = cache[key]
        if not pairs:
            continue
        for number, line in enumerate(text.split("\n"), 1):
            if len(line) > 600 or not line.strip():
                continue
            for cid, pattern in pairs:
                if pattern["needle"] and pattern["needle"] not in line:
                    continue
                if pattern["re"].search(line):
                    hits.setdefault(cid, []).append((rel, number, pattern["name"], line.strip()[:160]))
    for cls in catalog.classes:
        for spec in cls.get("files", []):
            if not spec.get("glob"):
                continue
            for rel in texts:
                if basename_matches(rel, spec["glob"]) and not rel.endswith((".example", ".sample", ".template")):
                    hits.setdefault(cls["id"], []).append((rel, 0, spec["name"], spec.get("note", "")))
    return hits


# ----------------------------------------------------------------------------- state

class State:
    """The audit's files under <out>: recon.json (facts), coverage.json (the units), hits/<unit>.json,
    findings.json, finding-<n>.json drafts, REPORT.md."""

    def __init__(self, root, out):
        self.root = root
        self.out = out if os.path.isabs(out) else os.path.join(root, out)
        self.out_rel = out if os.path.isabs(out) else out
        self.drafts = os.path.join(root, drafts_dir())
        self.drafts_rel = drafts_dir()

    def path(self, name):
        return os.path.join(self.out, name)

    def load(self, name, default):
        try:
            with open(self.path(name), encoding="utf-8") as handle:
                return json.load(handle)
        except (OSError, ValueError):
            return default

    def save(self, name, data):
        os.makedirs(self.out, exist_ok=True)
        tmp = self.path(name) + ".tmp"
        with open(tmp, "w", encoding="utf-8") as handle:
            json.dump(data, handle, indent=1, ensure_ascii=False)
        os.replace(tmp, self.path(name))

    def coverage(self):
        data = self.load("coverage.json", None)
        if data is None:
            fail("no audit here yet", "python3 %s recon" % sys.argv[0])
        return data

    def unit(self, coverage, uid):
        uid = uid.upper()
        for unit in coverage["units"]:
            if unit["id"] == uid:
                return unit
        known = ", ".join(u["id"] for u in coverage["units"])
        fail("no unit %s (units: %s)" % (uid, known), "python3 %s status" % sys.argv[0])

    def findings(self):
        return self.load("findings.json", [])


def fail(message, next_step=None):
    print(message)
    if next_step:
        print("Next: " + next_step)
    sys.exit(1)


def tool():
    """How the model invokes this tool (its own path, as given on the command line)."""
    return "python3 %s" % sys.argv[0]


def default_out():
    """Where the audit's state and report live: under the app's home (commands may write there and
    nothing cleans it; the project's `tmp/` is wiped when a turn ends), one folder per project."""
    home = os.environ.get("CORTEX_HOME")
    if not home:
        return OUT_DEFAULT
    root = os.getcwd()
    return os.path.join(home, "audits", "%s-%s" % (os.path.basename(root) or "project", hashlib.sha1(root.encode("utf-8")).hexdigest()[:8]))


def drafts_dir():
    """Where record templates are written for the model to fill with `edit` (which writes only inside
    the workspace): the project's scratch folder."""
    return OUT_DEFAULT


def parse_flags(args, flags_with_values):
    """Split `args` into positionals and {flag: value|True}."""
    positional, flags = [], {}
    i = 0
    while i < len(args):
        arg = args[i]
        if arg.startswith("--"):
            name = arg[2:]
            if name in flags_with_values:
                values = []
                while i + 1 < len(args) and not args[i + 1].startswith("--"):
                    values.append(args[i + 1])
                    i += 1
                flags[name] = values if flags_with_values[name] == "many" else (values[0] if values else True)
            else:
                flags[name] = True
        else:
            positional.append(arg)
        i += 1
    return positional, flags


# ----------------------------------------------------------------------------- recon

def cmd_recon(args):
    positional, flags = parse_flags(args, {"scope": "many", "out": "one"})
    scope = list(flags.get("scope", [])) + positional
    out = flags.get("out") or default_out()
    root = os.getcwd()
    started = time.time()
    state = State(root, out)
    catalog = Catalog()
    files = repo_files(root, scope)
    if not files:
        fail("no files to audit under %s%s" % (root, (" within " + ", ".join(scope)) if scope else ""), "run from the repository root, or name existing paths after --scope")
    git_head = (git(["rev-parse", "--short", "HEAD"], root) or "").strip()
    dirty = bool((git(["status", "--porcelain"], root) or "").strip())
    earlier_run = archive_previous(state)
    facts, texts = collect(root, files)
    hits = scan_texts(catalog, texts)
    units = []
    if earlier_run and earlier_run["issues"]:
        units.append({"id": "U0", "class": "previous", "name": "Issues of the previous audit", "reason": "%d issues reported on %s" % (len(earlier_run["issues"]), earlier_run["archived"][:8]), "status": "planned", "hits": len(earlier_run["issues"]), "files": len({i.get("file") for i in earlier_run["issues"] if i.get("file")}), "note": ""})
    for cls in catalog.classes:
        ok, reason = applies_to(cls, facts, texts)
        if not ok:
            continue
        uid = "U%d" % (len(units) + 1)
        unit_hits = hits.get(cls["id"], [])
        units.append({"id": uid, "class": cls["id"], "name": cls["name"], "reason": reason, "status": "planned", "hits": len(unit_hits), "files": len({h[0] for h in unit_hits}), "note": ""})
    os.makedirs(state.path("hits"), exist_ok=True)
    for unit in units:
        state.save(os.path.join("hits", unit["id"] + ".json"), hits.get(unit["class"], []))
    previous = state.load("coverage.json", None)
    coverage = {"root": root, "scope": scope, "started": time.strftime("%Y-%m-%d %H:%M"), "units": units, "files": len(files)}
    if previous and previous.get("units"):
        done = {u["class"]: u for u in previous["units"] if u["status"] not in ("planned", "in_progress")}
        for unit in units:
            if unit["class"] in done:
                unit["previous"] = {"status": done[unit["class"]]["status"], "note": done[unit["class"]]["note"]}
    state.save("coverage.json", coverage)
    stack = stack_of(facts)
    facts_out = {k: (sorted(v) if isinstance(v, set) else v) for k, v in facts.items() if k != "gitignore"}
    facts_out["stack"] = stack
    facts_out["files"] = len(files)
    facts_out["git_head"] = git_head
    facts_out["dirty"] = dirty
    state.save("recon.json", facts_out)

    # The map: what the project is, structurally.
    langs = sorted(facts["langs"].items(), key=lambda kv: -kv[1])
    code = [(LANG_NAMES.get(l, l), n) for l, n in langs if l in CODE_LANGS]
    other = [(LANG_NAMES.get(l, l), n) for l, n in langs if l not in CODE_LANGS]
    print("Project: %s%s, %d files%s%s" % (os.path.basename(root), (" at " + facts_out["git_head"]) if facts_out["git_head"] else "", len(files), " (scope: %s)" % ", ".join(scope) if scope else "", ", uncommitted changes present" if facts_out["dirty"] else ""))
    if code:
        print("Code: " + ", ".join("%s (%d)" % c for c in code[:6]))
    if other:
        print("Also: " + ", ".join("%s (%d)" % c for c in other[:6]))
    if stack:
        print("Stack: " + ", ".join(stack))
    if facts["entries"]:
        print("Entry points: " + ", ".join(facts["entries"][:8]) + (" ..." if len(facts["entries"]) > 8 else ""))
    routes = [h for h in hits.get("access-control", []) if h[2] == "route handler"]
    if routes:
        by_file = {}
        for h in routes:
            by_file[h[0]] = by_file.get(h[0], 0) + 1
        top = sorted(by_file.items(), key=lambda kv: -kv[1])[:5]
        print("Handlers: %d in %d files (%s)" % (len(routes), len(by_file), ", ".join("%s %d" % t for t in top)))
    manifests = facts["manifests"]
    if manifests:
        parts = []
        for rel, kind in manifests[:8]:
            locks = MANIFEST_LOCK.get(os.path.basename(rel), [])
            folder = os.path.dirname(rel)
            locked = any(os.path.dirname(lf) == folder and os.path.basename(lf) in locks for lf in facts["lockfiles"]) if locks else None
            parts.append(rel + ("" if locked is None else (" (locked)" if locked else " (NO LOCKFILE)")))
        print("Dependencies: " + ", ".join(parts))
    if facts["ci"] or facts["containers"] or facts["iac"]:
        print("Delivery: " + ", ".join(facts["ci"][:4] + facts["containers"][:4] + facts["iac"][:4]) + (" ..." if len(facts["ci"]) + len(facts["containers"]) + len(facts["iac"]) > 12 else ""))
    if facts["secret_files"]:
        print("Secret-looking files in the repository: " + ", ".join(facts["secret_files"][:8]))
    if not facts["tests"]:
        print("Tests: none found (local checks will be small scripts in tmp/)")
    for line in live_deps(state, root, files, shown=RECON_DEPS_SHOWN):
        print(line)
    if earlier_run and earlier_run["issues"]:
        recheck_previous(state, earlier_run, list(project_packages(root, files)))
        print("")
        print("A previous audit of this project (%s) reported %d issues; they are U0, re-checked first. Its report is kept at %s." % (earlier_run["archived"][:8], len(earlier_run["issues"]), state.path(earlier_run["report"])))
    print("")
    print("Units (the audit's checklist; each is one attack class that applies here):")
    for unit in units:
        line = "  %s %s: %s; %d suspicious lines in %d files" % (unit["id"], unit["class"], unit["reason"], unit["hits"], unit["files"])
        if unit.get("previous"):
            line += "; last audit: %s" % unit["previous"]["status"]
        print(line)
    print("")
    print("State kept in %s (survives the turn; the project itself is not written). %.1fs." % (state.out, time.time() - started))
    print("Next: %s next" % tool())


# ----------------------------------------------------------------------------- hunt, cover, status

def history_secrets(root, catalog):
    """Secret patterns in the last commits' diffs: (short sha, file, pattern name) rows."""
    log = git(["log", "-p", "-n", str(HISTORY_COMMITS), "--no-color", "--format=commit %h", "--", "."], root, limit=30_000_000)
    if not log:
        return []
    secret_patterns = [p for p in catalog.by_id["obvious"]["patterns"] if any(w in p["name"] for w in ("key", "token", "secret", "password", "connection", "JWT"))]
    rows, sha, path = [], "", ""
    for line in log.split("\n"):
        if line.startswith("commit "):
            sha = line[7:].strip()
        elif line.startswith("+++ b/"):
            path = line[6:].strip()
        elif line.startswith("+") and not line.startswith("+++"):
            for pattern in secret_patterns:
                if pattern["needle"] in line and pattern["re"].search(line):
                    rows.append((sha, path, pattern["name"]))
                    break
        if len(rows) >= 30:
            break
    return rows


def cmd_hunt(args):
    if not args:
        fail("which unit?", "%s hunt U1" % tool())
    state = State(os.getcwd(), default_out())
    coverage = state.coverage()
    unit = state.unit(coverage, args[0])
    catalog = Catalog()
    cls = catalog.by_id[unit["class"]]
    hits = state.load(os.path.join("hits", unit["id"] + ".json"), [])
    if unit["status"] == "planned":
        unit["status"] = "in_progress"
        state.save("coverage.json", coverage)
    print("%s %s (%s)" % (unit["id"], cls["name"], unit["reason"]))
    print("Invariant: " + cls["invariant"])
    print("Look for:")
    for item in cls["look"]:
        print("  - " + item)
    print("A finding here: " + cls["counts"])
    print("Only a hardening note: " + cls["hardening"])
    if cls.get("live") == "osv":
        for line in live_deps(state, state.root, repo_files(state.root, coverage.get("scope") or [])):
            print(line)
        print("")
        print("Each vulnerable package: is the affected code called with outside data? (read graph://used-by/<function>, grep -rn \"<package>\"). Reachable: a confirmed finding with the upgrade as the fix; not reachable: a hardening note naming the upgrade.")
        print("Next, when every package is settled: %s cover %s covered|candidate|blocked \"<what settled it>\"" % (tool(), unit["id"]))
        return
    if not offline_here(state.root):
        guidance = upstream_guidance(unit["class"])
        if guidance:
            print("Upstream guidance (Cloudflare security-audit skill, live):")
            for line in guidance:
                print("  " + line)
    if unit["class"] == "obvious":
        recon = state.load("recon.json", {})
        for rel in recon.get("secret_files", []):
            print("Secret-looking file: %s" % rel)
        rows = history_secrets(state.root, catalog)
        if rows:
            print("Secret patterns in the last %d commits (check whether they are real and still valid; a rotated secret is still a finding to report):" % HISTORY_COMMITS)
            for sha, path, name in rows[:12]:
                print("  %s %s: %s" % (sha, path, name))
    if not hits:
        print("No suspicious lines matched this class's shapes: read the 'look for' items against the entry points and the handlers, then cover the unit.")
    else:
        by_pattern = {}
        for rel, number, name, text in hits:
            by_pattern.setdefault(name, []).append((rel, number, text))
        print("Suspicious lines (%d in %d files), by shape:" % (len(hits), len({h[0] for h in hits})))
        shown = 0
        for name, rows in sorted(by_pattern.items(), key=lambda kv: -len(kv[1])):
            print("  %s (%d):" % (name, len(rows)))
            for rel, number, text in rows[:HITS_PER_PATTERN]:
                if shown >= HITS_SHOWN:
                    break
                print("    %s%s  %s" % (rel, ":%d" % number if number else "", text[:110]))
                shown += 1
            if len(rows) > HITS_PER_PATTERN:
                print("    ... %d more" % (len(rows) - HITS_PER_PATTERN))
        if shown < len(hits):
            listing = state.path(os.path.join("hits", unit["id"] + ".txt"))
            with open(listing, "w", encoding="utf-8") as handle:
                for rel, number, name, text in hits:
                    handle.write("%s:%d  [%s]  %s\n" % (rel, number, name, text))
            print("All %d lines: run `cat %s`" % (len(hits), listing))
    print("")
    print("For each line: find where its value comes from (read graph://used-by/<function>, read <path>:<from>-<to>), decide whether a lower-trust party controls it and whether a check you read stops it; then record (%s new <verdict>) or move on." % tool())
    print("Next, when every line is settled: %s cover %s covered|candidate|blocked \"<what settled it>\"" % (tool(), unit["id"]))


def cmd_cover(args):
    if len(args) < 2 or args[1] not in ("covered", "candidate", "blocked"):
        fail("usage: cover <unit> covered|candidate|blocked \"<note>\"", "%s status" % tool())
    state = State(os.getcwd(), default_out())
    coverage = state.coverage()
    unit = state.unit(coverage, args[0])
    status, note = args[1], " ".join(args[2:]).strip()
    if status == "blocked" and not note:
        fail("a blocked unit needs the note saying what could not be read or run", "%s cover %s blocked \"<why>\"" % (tool(), unit["id"]))
    if status == "candidate":
        recorded = [f for f in state.findings() if f.get("class") == unit["class"] or f.get("unit") == unit["id"]]
        if not recorded:
            fail("candidate means records exist for this class; none names %s yet" % unit["class"], "%s new confirmed|needs_validation|rejected, then finding <file>, then cover again" % tool())
    unit["status"], unit["note"] = status, note
    state.save("coverage.json", coverage)
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress")]
    print("%s %s: %s%s" % (unit["id"], unit["class"], status, (" (" + note + ")") if note else ""))
    if left:
        print("Left: " + ", ".join("%s %s" % (u["id"], u["class"]) for u in left))
        print("Next: %s hunt %s" % (tool(), left[0]["id"]))
    else:
        print("Every unit is settled.")
        print("Next: %s report" % tool())


def cmd_status(args):
    state = State(os.getcwd(), default_out())
    coverage = state.coverage()
    findings = state.findings()
    sync_units(state, coverage)
    states = hit_states(state)
    open_units = []
    for unit in coverage["units"]:
        rows = hit_rows(state, unit)
        left = sum(1 for r in rows if r[0] not in states)
        if left:
            open_units.append("%s %s (%d of %d lines open)" % (unit["id"], unit["class"], left, len(rows)))
    settled = [u for u in coverage["units"] if u["status"] in ("covered", "candidate", "blocked")]
    if open_units:
        print("Open: " + "; ".join(open_units[:4]) + ("; and %d more units" % (len(open_units) - 4) if len(open_units) > 4 else "") + ".")
        for unit in coverage["units"]:
            if unit.get("error"):
                print("Failed: %s %s: %s (still open)." % (unit["id"], unit["class"], unit["error"]))
    else:
        print("Settled: %s." % (", ".join("%s %s" % (u["id"], u["status"]) for u in settled) or "nothing yet"))
    counts = {v: sum(1 for f in findings if f.get("verdict") == v) for v in VERDICTS}
    if findings or not open_units:
        print("Records: %d confirmed, %d needs_validation, %d rejected" % (counts["confirmed"], counts["needs_validation"], counts["rejected"]))
    pending = [name for name, _ in pending_records(state)]
    if pending:
        print("Drafts not recorded yet: " + ", ".join(os.path.join(state.drafts_rel, d) for d in pending))
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress")]
    if pending or left or not os.path.isfile(state.path("REPORT.md")):
        print("Next: %s next" % tool())
        sys.exit(1)
    print("The audit is complete; the report is %s." % state.path("REPORT.md"))


# ----------------------------------------------------------------------------- findings

TEMPLATES = {
    "confirmed": {
        "verdict": "confirmed",
        "fingerprint": "? <class>:<file>:<function> (lowercase, no line numbers; the same for the same root cause)",
        "class": "? the unit's class id, e.g. injection",
        "title": "? one line: what a lower-trust party can do",
        "principal": "? who the attacker is and what they start with (an anonymous visitor, a logged-in user, a tenant member, another local user)",
        "trace": [
            {"kind": "entrypoint", "file": "? path", "line": 0, "what": "? where the value enters (the request field, argument, message)"},
            {"kind": "sink", "file": "? path", "line": 0, "what": "? what the value does there, unparameterized"}
        ],
        "control": "? the check that should have stopped it and why it does not (or 'none')",
        "impact": "? the concrete result for someone else or for the system",
        "conditions": [],
        "evidence": {"how": "? the run or test in tmp/ (command and input), or 'static: the code leaves no doubt because ...'", "observed": "? what the run showed, copied"},
        "severity": {"likelihood": "? informational|low|medium|high|critical", "impact": "? informational|low|medium|high|critical", "overall": "? never above impact"},
        "fix": {"where": "? path:function", "change": "? the smallest change at the last trusted point (the safe form of the sink)", "test": "? the regression test that would have caught it"}
    },
    "needs_validation": {
        "verdict": "needs_validation",
        "fingerprint": "? <class>:<file>:<function>",
        "class": "? class id",
        "title": "? one line: what would be possible if the missing fact goes the wrong way",
        "principal": "? who the attacker is and what they start with",
        "trace": [
            {"kind": "entrypoint", "file": "? path", "line": 0, "what": "? where the value enters"},
            {"kind": "sink", "file": "? path", "line": 0, "what": "? what it reaches"}
        ],
        "blocker": "? the one fact outside the code that decides (a proxy header, a deployment flag, a library version, a provider setting)",
        "check": "? how the owner verifies it without attacking anything (read a config, run a local fixture, ask the operator)"
    },
    "rejected": {
        "verdict": "rejected",
        "fingerprint": "? <class>:<file>:<function>",
        "class": "? class id",
        "title": "? one line: the claim that was checked",
        "trace": [
            {"kind": "sink", "file": "? path", "line": 0, "what": "? the suspicious line"}
        ],
        "reason": "? what disproves it: the check at path:line, or why there is no victim"
    }
}
SEVERITY_ANCHORS = [
    "critical: an anonymous party gets code execution, the whole data store, or any account",
    "high: an explicit control fully defeated with real consequence (auth bypass, another tenant's data read or written, stored script for other users, authenticated code execution)",
    "medium: a real boundary crossed with a small blast radius or rare preconditions",
    "low: non-secret internals disclosed, or much effort for little",
    "informational: confirmed but minimal; a step in something bigger",
]
REQUIRED = {
    "confirmed": ["fingerprint", "class", "title", "principal", "trace", "control", "impact", "evidence", "severity", "fix"],
    "needs_validation": ["fingerprint", "class", "title", "principal", "trace", "blocker", "check"],
    "rejected": ["fingerprint", "class", "title", "trace", "reason"],
}
FORBIDDEN = {
    "confirmed": ["blocker", "check", "reason"],
    "needs_validation": ["severity", "reason", "fix", "impact"],
    "rejected": ["severity", "fix", "blocker", "check", "impact", "evidence"],
}


def cmd_new(args):
    if not args or args[0] not in TEMPLATES:
        fail("usage: new confirmed|needs_validation|rejected", "%s new confirmed" % tool())
    state = State(os.getcwd(), default_out())
    os.makedirs(state.drafts, exist_ok=True)
    n = 1
    while os.path.exists(os.path.join(state.drafts, "finding-%d.json" % n)):
        n += 1
    name = "finding-%d.json" % n
    with open(os.path.join(state.drafts, name), "w", encoding="utf-8") as handle:
        json.dump(TEMPLATES[args[0]], handle, indent=1)
    rel = os.path.join(state.drafts_rel, name)
    print("Wrote %s. Replace every value that starts with '? ' using edit; keep the field names." % rel)
    if args[0] == "confirmed":
        print("Severity comes from the demonstrated result, never from the class name; overall never exceeds impact:")
        for anchor in SEVERITY_ANCHORS:
            print("  " + anchor)
    if args[0] == "needs_validation":
        print("No severity here: a lead with one missing fact is not a lower-severity finding.")
    print("Next: %s finding %s" % (tool(), rel))


def placeholders(value, path="record"):
    out = []
    if isinstance(value, dict):
        for key, item in value.items():
            out.extend(placeholders(item, path + "." + key))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            out.extend(placeholders(item, "%s[%d]" % (path, index)))
    elif isinstance(value, str) and value.startswith("? "):
        out.append(path)
    return out


def line_count(root, rel):
    text = read_text(os.path.join(root, rel))
    return None if text is None else text.count("\n") + 1


def validate(record, root, catalog):
    """The problems of a finding record, each with what to do; empty when it is sound."""
    problems = []
    if not isinstance(record, dict):
        return ["the file must hold one JSON object (the template's shape)"]
    verdict = record.get("verdict")
    if verdict not in VERDICTS:
        return ["verdict must be confirmed, needs_validation or rejected"]
    for key in REQUIRED[verdict]:
        if key not in record or record[key] in ("", [], {}, None):
            problems.append("%s is missing (required for %s)" % (key, verdict))
    for key in FORBIDDEN[verdict]:
        if key in record:
            problems.append("%s does not belong in a %s record: remove it" % (key, verdict))
    for key in ("unit", "hit"):
        if key in record and not isinstance(record[key], str):
            problems.append(key + " must be a string")
    for path in placeholders(record):
        problems.append("%s still holds the template's '? ...' placeholder" % path)
    fp = record.get("fingerprint", "")
    if isinstance(fp, str) and fp and not fp.startswith("? "):
        if not re.match(r"^[a-z0-9][a-z0-9._:/@+-]*$", fp):
            problems.append("fingerprint must be lowercase letters, digits and . _ : / @ + - only")
        if re.search(r":\d+(\b|$)", fp):
            problems.append("fingerprint carries a line number; use <class>:<file>:<function> so it survives edits")
    cls = record.get("class")
    if isinstance(cls, str) and cls and not cls.startswith("? ") and cls not in catalog.by_id:
        problems.append("class %s is not one of: %s" % (cls, ", ".join(sorted(catalog.by_id))))
    trace = record.get("trace")
    if isinstance(trace, list) and trace:
        for index, step in enumerate(trace):
            where = "trace[%d]" % index
            if not isinstance(step, dict):
                problems.append(where + " must be an object with kind, file, line, what")
                continue
            if step.get("kind") not in ("entrypoint", "propagation", "sink"):
                problems.append(where + ".kind must be entrypoint, propagation or sink")
            rel = step.get("file")
            line = step.get("line")
            if not isinstance(rel, str) or rel.startswith("? "):
                problems.append(where + ".file is missing")
            elif not os.path.isfile(os.path.join(root, rel)):
                problems.append("%s.file %s does not exist (repository-relative path)" % (where, rel))
            elif not isinstance(line, int) or line < 1:
                problems.append(where + ".line must be the 1-based line number (read the file to copy it)")
            else:
                total = line_count(root, rel)
                if total is not None and line > total:
                    problems.append("%s.line %d is past the end of %s (%d lines)" % (where, line, rel, total))
            if not isinstance(step.get("what"), str) or not step.get("what", "").strip() or step.get("what", "").startswith("? "):
                problems.append(where + ".what is missing")
        if len(trace) > 1:
            if trace[0].get("kind") != "entrypoint":
                problems.append("trace[0].kind must be entrypoint (where the value enters)")
            if trace[-1].get("kind") != "sink":
                problems.append("the last trace step must be the sink (where the value acts)")
    elif "trace" in record:
        problems.append("trace must be a non-empty list of steps")
    if verdict == "confirmed":
        severity = record.get("severity")
        if isinstance(severity, dict):
            for key in ("likelihood", "impact", "overall"):
                if severity.get(key) not in SEVERITY:
                    problems.append("severity.%s must be one of %s" % (key, ", ".join(SEVERITY)))
            if severity.get("impact") in SEVERITY and severity.get("overall") in SEVERITY and SEVERITY.index(severity["overall"]) > SEVERITY.index(severity["impact"]):
                problems.append("severity.overall (%s) is above severity.impact (%s): overall never exceeds the demonstrated impact" % (severity["overall"], severity["impact"]))
        elif "severity" in record:
            problems.append("severity must be an object with likelihood, impact and overall")
        evidence = record.get("evidence")
        if isinstance(evidence, dict):
            for key in ("how", "observed"):
                if not isinstance(evidence.get(key), str) or not evidence.get(key, "").strip() or evidence.get(key, "").startswith("? "):
                    problems.append("evidence.%s is missing (what you ran or read, and what it showed)" % key)
        elif "evidence" in record:
            problems.append("evidence must be an object with how and observed")
        fix = record.get("fix")
        if isinstance(fix, dict):
            for key in ("where", "change", "test"):
                if not isinstance(fix.get(key), str) or not fix.get(key, "").strip() or fix.get(key, "").startswith("? "):
                    problems.append("fix.%s is missing" % key)
        elif "fix" in record:
            problems.append("fix must be an object with where, change and test")
    for key in ("title", "principal", "impact", "control", "blocker", "check", "reason"):
        value = record.get(key)
        if key in record and (not isinstance(value, str) or not value.strip()):
            problems.append(key + " must be a non-empty sentence")
    return sorted(set(problems), key=problems.index)


def cmd_finding(args):
    if not args:
        fail("which file?", "%s finding tmp/secaudit/finding-1.json" % tool())
    root = os.getcwd()
    state = State(root, default_out())
    path = args[0]
    try:
        with open(os.path.join(root, path), encoding="utf-8") as handle:
            record = json.load(handle)
    except OSError:
        fail("cannot read %s" % path, "%s status" % tool())
    except ValueError as error:
        fail("%s is not valid JSON: %s" % (path, error), "fix the JSON with edit, then run this again")
    problems = validate(record, root, Catalog())
    if problems:
        print("%s is not ready:" % path)
        for problem in problems:
            print("  - " + problem)
        print("Next: fix these with edit, then %s finding %s" % (tool(), path))
        sys.exit(1)
    findings = state.findings()
    replaced = [f for f in findings if f.get("fingerprint") == record["fingerprint"]]
    findings = [f for f in findings if f.get("fingerprint") != record["fingerprint"]]
    record["recorded"] = time.strftime("%Y-%m-%d %H:%M")
    record["draft"] = path
    for step in record.get("trace", []):
        content = read_text(os.path.join(root, step["file"])) or ""
        lines = content.split("\n")
        if 0 < step.get("line", 0) <= len(lines):
            step["text"] = lines[step["line"] - 1].strip()
    findings.append(record)
    findings.sort(key=lambda f: (VERDICTS.index(f["verdict"]), -(SEVERITY.index(f["severity"]["overall"]) if f.get("severity") else -1), f["fingerprint"]))
    state.save("findings.json", findings)
    if record.get("hit"):
        states = hit_states(state)
        states[record["hit"]] = {"status": "finding" if record["verdict"] == "confirmed" else "lead" if record["verdict"] == "needs_validation" else "safe", "note": record["title"], "record": os.path.basename(path)}
        state.save("hits-state.json", states)
        coverage = state.load("coverage.json", None)
        if coverage:
            sync_units(state, coverage)
    print("Recorded %s as %s%s." % (record["fingerprint"], record["verdict"], " (replaced the earlier record)" if replaced else ""))
    coverage = state.load("coverage.json", None)
    unit = None
    if coverage:
        for u in coverage["units"]:
            if u["class"] == record["class"] or u["id"] == record.get("unit"):
                unit = u
                break
    print("Next: %s next" % tool())


# ----------------------------------------------------------------------------- report

def md_escape(text):
    return str(text).replace("|", "\\|").replace("\n", " ")


def cmd_report(args):
    positional, flags = parse_flags(args, {})
    state = State(os.getcwd(), default_out())
    coverage = state.coverage()
    recon = state.load("recon.json", {})
    findings = state.findings()
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress")]
    if left and not flags.get("partial"):
        print("Not every unit is settled: " + ", ".join("%s %s (%s)" % (u["id"], u["class"], u["status"]) for u in left))
        fail("hunt them, or mark them blocked with why; `report --partial` writes a report that says they were not covered", "%s hunt %s" % (tool(), left[0]["id"]))
    confirmed = [f for f in findings if f["verdict"] == "confirmed"]
    leads = [f for f in findings if f["verdict"] == "needs_validation"]
    rejected = [f for f in findings if f["verdict"] == "rejected"]
    confirmed.sort(key=lambda f: -SEVERITY.index(f["severity"]["overall"]))
    lines = []
    add = lines.append
    add("# Security audit: %s" % os.path.basename(coverage["root"]))
    add("")
    add("Source: %s%s. Scope: %s. %d files read. Method: source review of every attack class that applies to this stack, each suspicious line traced to its source and its effect; local checks with dummy data only; nothing deployed was touched." % (recon.get("git_head") or "working tree", " (uncommitted changes present)" if recon.get("dirty") else "", ", ".join(coverage.get("scope") or ["whole repository"]), coverage.get("files", 0)))
    if recon.get("stack"):
        add("Stack: " + ", ".join(recon["stack"]) + ".")
    if left:
        add("")
        add("**Partial pass.** Not covered: " + ", ".join("%s (%s)" % (u["class"], u["status"]) for u in left) + ".")
    blocked = [u for u in coverage["units"] if u["status"] == "blocked"]
    add("")
    add("## Summary")
    add("")
    counts = {level: sum(1 for f in confirmed if f["severity"]["overall"] == level) for level in SEVERITY}
    summary = ", ".join("%d %s" % (counts[level], level) for level in reversed(SEVERITY) if counts[level])
    summary_line = "%d confirmed finding%s%s; %d lead%s needing validation; %d claim%s checked and rejected. %d of %d units covered%s." % (len(confirmed), "" if len(confirmed) == 1 else "s", " (%s)" % summary if summary else "", len(leads), "" if len(leads) == 1 else "s", len(rejected), "" if len(rejected) == 1 else "s", sum(1 for u in coverage["units"] if u["status"] in ("covered", "candidate")), len(coverage["units"]), "; %d blocked" % len(blocked) if blocked else "")
    add(summary_line)
    add("")
    judged_all = hit_states(state)
    if judged_all:
        add("The reviewer traced %d flagged lines to their source and their effect; each verdict and its reasoning is in the unit reports below." % len(judged_all))
    add("This is one pass by one reviewer: a class marked covered means its suspicious lines were traced, not that it holds no defect.")
    add("")
    add("## Confirmed findings")
    add("")
    if not confirmed:
        add("None.")
    else:
        add("| Severity | Title | Where | Result |")
        add("|---|---|---|---|")
        for f in confirmed:
            sink = f["trace"][-1]
            add("| %s | %s | %s:%d | %s |" % (f["severity"]["overall"], md_escape(f["title"]), sink["file"], sink["line"], md_escape(f["impact"])))
        for f in confirmed:
            add("")
            add("### %s: %s" % (f["severity"]["overall"], f["title"]))
            add("")
            add("- Fingerprint: `%s`" % f["fingerprint"])
            add("- Attacker: " + f["principal"])
            add("- Path:")
            for step in f["trace"]:
                add("  - %s `%s:%d`: %s" % (step["kind"], step["file"], step["line"], step["what"]))
            add("- Control: " + f["control"])
            if f.get("conditions"):
                add("- Conditions: " + "; ".join(f["conditions"]))
            add("- Evidence: %s. Observed: %s" % (f["evidence"]["how"], f["evidence"]["observed"]))
            add("- Impact: " + f["impact"])
            add("- Severity: likelihood %s, impact %s, overall %s" % (f["severity"]["likelihood"], f["severity"]["impact"], f["severity"]["overall"]))
            add("- Fix: in %s, %s. Test: %s" % (f["fix"]["where"], f["fix"]["change"], f["fix"]["test"]))
    previous = state.load(PREVIOUS_FILE, None)
    if previous and previous.get("issues"):
        judged_prev = hit_states(state)
        add("")
        add("## Previous audit (%s)" % previous["archived"][:8])
        add("")
        fixed = sum(1 for i, issue in enumerate(previous["issues"], 1) if (judged_prev.get("U0.%d" % i) or {}).get("status") == "fixed")
        still = sum(1 for i, issue in enumerate(previous["issues"], 1) if (judged_prev.get("U0.%d" % i) or {}).get("status") in ("finding", "lead", "hardening"))
        add("The previous audit reported %d issues; %d are fixed, %d are still open, %d were not re-checked. Its report is kept at %s." % (len(previous["issues"]), fixed, still, len(previous["issues"]) - fixed - still, previous["report"]))
        add("")
        add("| Issue | Was | Where | Code now | Verdict |")
        add("|---|---|---|---|---|")
        for i, issue in enumerate(previous["issues"], 1):
            st = judged_prev.get("U0.%d" % i) or {}
            where = ("%s:%d" % (issue["file"], issue["line"])) if issue.get("file") and issue.get("line") else (issue.get("file") or "")
            add("| %s | %s | %s | %s | %s |" % (md_escape(issue["title"][:90]), issue["kind"] + ((" " + issue["severity"]) if issue.get("severity") else ""), where, md_escape(issue.get("now", "")), md_escape(VERDICT_WORDS.get(st.get("status"), st.get("status") or "not re-checked") + ((": " + st["note"]) if st.get("note") else ""))))
    deps = state.load("deps.json", {}) or {}
    if deps.get("rows"):
        add("")
        add("## Known vulnerable dependencies (OSV, %s)" % deps.get("checked", ""))
        add("")
        add("| Package | Version | Advisory | Severity | Fixed in | Lockfile |")
        add("|---|---|---|---|---|---|")
        for row in deps["rows"]:
            add("| %s %s | %s | %s%s %s | %s | %s | %s |" % (row["eco"], row["name"], row["version"], row["id"], (" / " + row["aliases"][0]) if row["aliases"] else "", md_escape(row["summary"]), row["severity"], ", ".join(sorted(set(row["fixed"]))[:3]) or "none listed", row["lockfile"]))
        add("")
        add("Reachability of each advisory is in the findings above when it was traced; an upgrade to the fixed version is the hardening note otherwise.")
    add("")
    add("## Needs validation")
    add("")
    if not leads:
        add("None.")
    else:
        add("No severity: each lead waits on one fact outside the code.")
        add("")
        add("| Title | Where | Missing fact | How to check |")
        add("|---|---|---|---|")
        for f in leads:
            sink = f["trace"][-1]
            add("| %s | %s:%d | %s | %s |" % (md_escape(f["title"]), sink["file"], sink["line"], md_escape(f["blocker"]), md_escape(f["check"])))
    hardening = [(u["class"], u["note"]) for u in coverage["units"] if u["status"] == "covered" and u.get("note")]
    add("")
    add("## Hardening notes")
    add("")
    if not hardening:
        add("None recorded.")
    else:
        for cls, note in hardening:
            add("- %s: %s" % (cls, note))
    judged = hit_states(state)
    if judged:
        add("")
        add("## Lines judged")
        add("")
        add("Every line the tool showed and how the reviewer settled it. `safe` names the check or fact; `hardening` a change worth making without a reachable attack; `blocked` what could not be read or run.")
        for u in coverage["units"]:
            rows = [(hid, rel, number, shape, text) for hid, rel, number, shape, text in hit_rows(state, u) if hid in judged]
            if not rows:
                continue
            add("")
            add("### %s %s" % (u["id"], u["class"]))
            add("")
            add("| Line | Where | Shape | Verdict | Reason |")
            add("|---|---|---|---|---|")
            for hid, rel, number, shape, text in rows:
                st = judged[hid]
                where = ("%s:%d" % (rel, number)) if number else (rel or "")
                add("| %s | %s | %s | %s | %s |" % (hid, where, md_escape(shape), st["status"], md_escape(st.get("note", ""))))
    catalog_for_units = Catalog()
    unit_files = []
    for u in coverage["units"]:
        if u["status"] in ("covered", "candidate", "blocked", "in_progress"):
            unit_files.append((u, unit_report(state, coverage, u, catalog_for_units)))
    if unit_files:
        add("")
        add("## Unit reports")
        add("")
        add("One file per attack class, with every reviewed line, its verdict and the reviewer's reasoning:")
        add("")
        for u, name in unit_files:
            add("- [%s %s](units/%s): %s" % (u["id"], u["class"], name, u["status"].replace("_", " ")))
    add("")
    add("## Coverage")
    add("")
    add("| Unit | Class | Why it applies | Lines reviewed | Status | Note |")
    add("|---|---|---|---|---|---|")
    for u in coverage["units"]:
        add("| %s | %s | %s | %d | %s | %s |" % (u["id"], u["class"], md_escape(u["reason"]), u["hits"], u["status"], md_escape(unit_note(u))))
    if rejected:
        add("")
        add("## Rejected claims")
        add("")
        add("Checked and disproved; kept so the next audit does not repeat them.")
        add("")
        for f in rejected:
            add("- `%s` %s: %s" % (f["fingerprint"], f["title"], f["reason"]))
    add("")
    os.makedirs(state.out, exist_ok=True)
    with open(state.path("REPORT.md"), "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))
    rel = state.path("REPORT.md")
    print("Wrote %s." % rel)
    print(summary_line)
    for f in confirmed:
        sink = f["trace"][-1]
        print("  %s: %s (%s:%d). Fix: %s" % (f["severity"]["overall"], f["title"], sink["file"], sink["line"], f["fix"]["change"]))
    for f in leads:
        sink = f["trace"][-1]
        print("  needs validation: %s (%s:%d); missing: %s" % (f["title"], sink["file"], sink["line"], f["blocker"]))
    for u in blocked + left:
        print("  not covered: %s %s%s" % (u["id"], u["class"], (": " + u["note"]) if u.get("note") else ""))
    print("The report is at %s." % rel)


# ----------------------------------------------------------------------------- scan

def changed_files(root, ref):
    """Files changed against `ref` (default HEAD), plus untracked ones."""
    out = git(["diff", "--name-only", "--diff-filter=ACMR", ref or "HEAD"], root)
    if out is None:
        fail("not a git repository, or %s is not a ref" % (ref or "HEAD"), "%s scan <path>..." % tool())
    files = [rel for rel in out.split("\n") if rel.strip()]
    untracked = git(["ls-files", "--others", "--exclude-standard"], root) or ""
    files.extend(rel for rel in untracked.split("\n") if rel.strip())
    return sorted(set(files))


def cmd_scan(args):
    positional, flags = parse_flags(args, {"changed": "one"})
    root = os.getcwd()
    if "changed" in flags:
        ref = flags["changed"] if isinstance(flags["changed"], str) else None
        rels = changed_files(root, ref)
        label = "changed against %s" % (ref or "HEAD")
    else:
        if not positional:
            fail("usage: scan <path>... | scan --changed [<ref>]", "%s scan --changed" % tool())
        rels = []
        for p in positional:
            full = os.path.join(root, p)
            if os.path.isdir(full):
                rels.extend(repo_files(root, [p.rstrip("/")]))
            elif os.path.isfile(full):
                rels.append(os.path.relpath(full, root))
            else:
                fail("%s does not exist" % p, "name repository paths")
        label = ", ".join(positional)
    rels = [rel for rel in sorted(set(rels)) if not skipped(rel) and os.path.isfile(os.path.join(root, rel))]
    if not rels:
        print("Nothing to scan (%s)." % label)
        return
    catalog = Catalog()
    texts = {}
    for rel in rels:
        text = read_text(os.path.join(root, rel))
        if text is not None:
            texts[rel] = (lang_of(rel), text)
    hits = scan_texts(catalog, texts)
    print("%d files (%s)." % (len(texts), label))
    if not hits:
        print("No suspicious lines matched any class's shapes. Still read each change for: a new handler without an authorization check, a request field reaching old code, a removed condition, a new dependency, a changed default.")
        return
    total = 0
    for cls in catalog.classes:
        rows = hits.get(cls["id"])
        if not rows:
            continue
        print("")
        print("%s: %s" % (cls["name"], cls["counts"]))
        for rel, number, name, text in rows[:HITS_SHOWN]:
            print("  %s%s  [%s]  %s" % (rel, ":%d" % number if number else "", name, text[:100]))
        if len(rows) > HITS_SHOWN:
            print("  ... %d more" % (len(rows) - HITS_SHOWN))
        total += len(rows)
    print("")
    print("For each line: where does the value come from (read graph://used-by/<function>, read <path>:<from>-<to>), who controls it, which check stops it. A hit is a question, not a finding.")


# ----------------------------------------------------------------------------- next and settle: the script drives

HIT_STATUSES = ("safe", "hardening", "lead", "finding", "blocked", "fixed", "still")
HITS_PER_STEP = 30
DEPS_PER_STEP = 60


def hit_rows(state, unit):
    """The unit's hits as the model settles them: [(id, file, line, shape, text)]. The dependencies
    unit's hits are its vulnerable packages; the previous-issues unit's are the last audit's issues;
    a unit without shape hits has one hit, the unit itself."""
    if unit["class"] == "previous":
        return previous_rows(state, unit) or [("%s.0" % unit["id"], "", 0, "the unit", "the previous audit left no issue to re-check; settle the unit")]
    if unit["class"] == "dependencies":
        rows = (state.load("deps.json", {}) or {}).get("rows") or []
        groups = {}
        for row in rows:
            groups.setdefault((row["eco"], row["name"], row["version"], row["lockfile"]), []).append(row)
        ordered = sorted(groups.items(), key=lambda kv: (min(SEVERITY_RANK.get(r["severity"], 5) for r in kv[1]), kv[0][1]))
        out = []
        for index, ((eco, name, version, lockfile), group) in enumerate(ordered, 1):
            fix = covering_fix(group)
            text = "%s %s %s: %d advisor%s, worst %s; %s; %s" % (eco, name, version, len(group), "y" if len(group) == 1 else "ies", group[0]["severity"], ("upgrade to %s" % fix) if fix else "no fixed version", "; ".join("%s %s" % (r["id"], r["summary"][:70]) for r in group[:2]))
            out.append(("%s.%d" % (unit["id"], index), lockfile, 0, "vulnerable package", text))
        if out:
            return out
        return [("%s.0" % unit["id"], "", 0, "the unit", "no pinned dependency has a known vulnerability (OSV, live); settle the unit")]
    try:
        hits = state.load(os.path.join("hits", unit["id"] + ".json"), [])
        rows = [("%s.%d" % (unit["id"], index), rel, number, name, text) for index, (rel, number, name, text) in enumerate(hits, 1)]
    except (OSError, ValueError, TypeError) as error:
        # A damaged hits file never hides the unit: it stays one open line the model settles.
        return [("%s.0" % unit["id"], "", 0, "the unit", "the unit's lines could not be read (%s): settle the unit blocked with that reason, or run recon again" % error)]
    if not rows:
        return [("%s.0" % unit["id"], "", 0, "the unit", "no code shape of this class matched: read its 'look for' items against the entry points and handlers, then settle the unit")]
    return rows


def unit_note(unit):
    """A unit's note for the reports: what the model noted, and the tool's own failure when it had one."""
    parts = [unit.get("note", "")]
    if unit.get("error"):
        parts.append("the tool failed on this unit: %s" % unit["error"])
    return "; ".join(p for p in parts if p)


LOCATION = re.compile(r"((?:[\w.-]+/)*[\w.-]+\.?[\w-]*):(\d+)")


def cited_location(state, note, targets):
    """The place the note cites for a judgment of several lines: a `path:line` that exists in the
    project, or, for the dependencies unit, one of the packages or lockfiles judged. None when the
    note cites nothing that can be checked."""
    units = {u["id"]: u for u, _ in targets}
    if any(u["class"] == "dependencies" for u in units.values()):
        words = set(re.findall(r"[\w@./-]+", note.lower()))
        for _, (hid, rel, number, shape, text) in targets:
            head = text.split(":")[0]
            package = head.split(" ")[1] if " " in head else head
            for candidate in (package.lower(), package.split("/")[-1].lower(), (rel or "").lower()):
                if candidate and candidate in words:
                    return candidate
        return None
    for match in LOCATION.finditer(note):
        rel, number = match.group(1).rstrip("."), int(match.group(2))
        full = os.path.join(state.root, rel)
        if not os.path.isfile(full):
            continue
        text = read_text(full)
        if text is not None and 1 <= number <= max(1, text.count("\n") + 1):
            return "%s:%d" % (rel, number)
    return None


def hit_states(state):
    return state.load("hits-state.json", {})


def unit_status_from_hits(rows, states):
    """A unit's status from its hits: planned (none settled), in_progress, blocked, candidate or covered."""
    settled = [states.get(hid) for hid, *_ in rows]
    if all(s is None for s in settled):
        return "planned"
    if any(s is None for s in settled):
        return "in_progress"
    kinds = {s["status"] for s in settled}
    if kinds & {"finding", "lead"}:
        return "candidate"
    if "blocked" in kinds:
        return "blocked"
    return "covered"


def sync_units(state, coverage):
    """Write every unit's status and note from its hits (the notes of safe, hardening and blocked hits)."""
    states = hit_states(state)
    for unit in coverage["units"]:
        rows = hit_rows(state, unit)
        status = unit_status_from_hits(rows, states)
        if status == "planned":
            # No line judged through settle: the unit keeps what hunt and cover gave it.
            continue
        unit["status"] = status
        notes = []
        for hid, *_ in rows:
            s = states.get(hid)
            if s and s.get("note") and s["status"] in ("hardening", "blocked"):
                notes.append(s["note"])
        if notes:
            unit["note"] = "; ".join(dict.fromkeys(notes))
    state.save("coverage.json", coverage)


def pending_records(state):
    """Record files written for hits and not yet recorded."""
    recorded = {f.get("hit") for f in state.findings()}
    out = []
    if os.path.isdir(state.drafts):
        for name in sorted(os.listdir(state.drafts)):
            if re.match(r"finding-\d+\.json$", name):
                try:
                    with open(os.path.join(state.drafts, name), encoding="utf-8") as handle:
                        record = json.load(handle)
                except (OSError, ValueError):
                    continue
                if record.get("hit") and record.get("hit") not in recorded:
                    out.append((name, record["hit"]))
    return out


def print_unit_step(state, coverage, unit, catalog, first_time):
    cls = catalog.by_id[unit["class"]]
    states = hit_states(state)
    rows = hit_rows(state, unit)
    open_rows = [r for r in rows if r[0] not in states]
    print("%s %s (%s): %d of %d lines open." % (unit["id"], cls["name"], unit["reason"], len(open_rows), len(rows)))
    if first_time:
        print("Invariant: " + cls["invariant"])
        print("Look for:")
        for item in cls["look"]:
            print("  - " + item)
        print("A finding here: " + cls["counts"])
        print("Only a hardening note: " + cls["hardening"])
        if not offline_here(state.root):
            guidance = upstream_guidance(unit["class"])
            if guidance:
                print("Upstream guidance (Cloudflare security-audit skill, live):")
                for line in guidance:
                    print("  " + line)
    shown = DEPS_PER_STEP if unit["class"] == "dependencies" else HITS_PER_STEP
    # A file's run of lines with the same shape is one entry with an id range: one judgment
    # settles them together (seen: 61 innerHTML lines of two static design files judged one by one).
    entries = []
    index = 0
    while index < len(open_rows):
        hid, rel, number, shape, text = open_rows[index]
        run = [open_rows[index]]
        while index + len(run) < len(open_rows) and open_rows[index + len(run)][1] == rel and open_rows[index + len(run)][3] == shape:
            run.append(open_rows[index + len(run)])
        if len(run) >= 3:
            entries.append(("%s-%s" % (run[0][0], run[-1][0]), rel, 0, shape, "%d lines, e.g. :%d %s" % (len(run), number, text[:110])))
        else:
            entries.extend(run)
        index += len(run)
    print("Lines to judge now:")
    for hid, rel, number, shape, text in entries[:shown]:
        where = ("%s:%d" % (rel, number)) if number else rel
        print("  %s  %s  [%s]  %s" % (hid, where, shape, text[:150]))
    if len(entries) > shown:
        print("  ... %d more come with the next `next` once these are settled (one control that covers a whole file or the unit settles them together: settle %s safe \"<the control>\")" % (len(entries) - shown, unit["id"]))
    if any("-" in e[0] for e in entries[:shown]):
        print("  A range (U3.4-U3.20) is one file's lines of one shape: settle the range with one reason, or a single id inside it.")
    print("")
    print("For each line: where does the value come from (read <path>:<from>-<to> around it, read graph://used-by/<function>), who controls it, which check stops it. Then settle it:")
    print("  %s settle %s safe \"<the check or fact that settles it, with path:line>\"" % (tool(), open_rows[0][0] if open_rows else unit["id"]))
    print("  %s settle <ids...> hardening \"<the note, e.g. the upgrade>\" | blocked \"<what could not be read or run>\"" % tool())
    print("  %s settle <id> finding   (a confirmed vulnerability)  |  lead   (one fact outside the code decides)" % tool())
    if unit["class"] == "previous":
        print("  %s settle <ids...> fixed \"<what changed, with path:line>\"  |  still \"<re-verified how>\"   (a still finding or lead is carried into this report as it was)" % tool())
    print("Several ids share one reason: settle U3.1 U3.2 U3.5 safe \"...\". A finding or lead gets a record file to fill before the next step.")
    print("Next: settle the lines above, then %s next" % tool())


def cmd_next(args):
    root = os.getcwd()
    state = State(root, default_out())
    catalog = Catalog()
    coverage = state.load("coverage.json", None)
    if coverage is not None and os.path.isfile(state.path("REPORT.md")) and not coverage.get("report_stale", True):
        # The last audit of this project is complete: this is a new run. recon archives the old one
        # and seeds U0 with its issues.
        coverage = None
    if coverage is None:
        cmd_recon([])
        coverage = state.coverage()
        print("")
    pending = pending_records(state)
    if pending:
        name, hid = pending[0]
        print("The record for %s is not written yet: %s still holds its template." % (hid, os.path.join(state.drafts_rel, name)))
        print("Next: fill every '? ' value with edit, then %s finding %s" % (tool(), os.path.join(state.drafts_rel, name)))
        sys.exit(1)
    sync_units(state, coverage)
    states = hit_states(state)
    open_units = [u for u in coverage["units"] if any(r[0] not in states for r in hit_rows(state, u))]
    # A unit whose step failed is shown after the others, and never dropped: it stays open until
    # its lines are settled, and comes back for a retry; a second failure hands the model the
    # settle that records the failure in the report.
    open_units.sort(key=lambda u: 1 if u.get("error") else 0)
    for position, unit in enumerate(open_units):
        first_time = unit["status"] == "planned"
        try:
            if first_time:
                unit["status"] = "in_progress"
                state.save("coverage.json", coverage)
            print_unit_step(state, coverage, unit, catalog, first_time)
        except SystemExit:
            raise
        except Exception as error:  # noqa: BLE001 - any failure of one unit's step
            message = "%s: %s" % (type(error).__name__, str(error)[:200])
            failed_before = unit.get("error")
            unit["error"] = message
            state.save("coverage.json", coverage)
            print("")
            if failed_before or position == len(open_units) - 1:
                print("%s %s could not be shown%s: %s" % (unit["id"], unit["class"], " again" if failed_before else "", message))
                print("The unit stays open until its lines are settled; the report lists it as not assessed with this reason.")
                print("Next: %s settle %s blocked \"the tool failed on this unit: %s\"" % (tool(), unit["id"], message.replace('"', "'")))
                sys.exit(1)
            print("%s %s could not be shown (%s); it stays open and comes back after the other units." % (unit["id"], unit["class"], message))
            print("")
            continue
        if unit.pop("error", None) is not None:
            state.save("coverage.json", coverage)
        return
    if not os.path.isfile(state.path("REPORT.md")) or coverage.get("report_stale", True):
        coverage["report_stale"] = False
        state.save("coverage.json", coverage)
        print("Every line of every unit is settled. Writing the report.")
        cmd_report([])
        print("")
        print("Next: %s next" % tool())
        return
    report_path = state.path("REPORT.md")
    print("The audit is complete. The report (%s):" % report_path)
    print("")
    with open(report_path, encoding="utf-8") as handle:
        print(handle.read())
    print("Next: reply to the user with the report's summary, every confirmed finding (severity, title, path:line, result, fix), every lead with its missing fact, what was not covered, and the report's path %s." % report_path)


def cmd_settle(args):
    if len(args) < 2:
        fail("usage: settle <id or unit>... safe|hardening|lead|finding|blocked [\"note\"]", "%s next" % tool())
    state = State(os.getcwd(), default_out())
    coverage = state.coverage()
    catalog = Catalog()
    ids, status, note = [], None, ""
    for index, arg in enumerate(args):
        if arg in HIT_STATUSES:
            status, note = arg, " ".join(args[index + 1:]).strip().strip('"')
            break
        ids.append(arg.upper())
    if status is None or not ids:
        fail("usage: settle <id or unit>... safe|hardening|lead|finding|blocked [\"note\"]", "%s next" % tool())
    if status in ("fixed", "still") and not all(i.startswith("U0") for i in ids):
        fail("fixed and still are for the previous audit's issues (U0) only", "%s next" % tool())
    if status in ("safe", "hardening", "blocked", "fixed", "still") and len(note) < 8:
        fail("%s needs the reason (the check or fact, with path:line)" % status, "%s settle %s %s \"<reason>\"" % (tool(), " ".join(ids), status))
    states = hit_states(state)
    units = {u["id"]: u for u in coverage["units"]}
    targets = []
    for ident in ids:
        unit_id = ident.split(".")[0]
        unit = units.get(unit_id)
        if unit is None:
            fail("no unit %s" % unit_id, "%s next" % tool())
        rows = hit_rows(state, unit)
        if "-" in ident:
            first, last = ident.split("-", 1)
            if "." not in last:
                last = unit_id + "." + last
            index_of = {r[0]: i for i, r in enumerate(rows)}
            if first not in index_of or last not in index_of or index_of[first] > index_of[last]:
                fail("no range %s in %s (%s to %s)" % (ident, unit_id, rows[0][0], rows[-1][0]), "%s next" % tool())
            targets.extend((unit, r) for r in rows[index_of[first]:index_of[last] + 1] if r[0] not in states)
            continue
        if "." in ident:
            row = next((r for r in rows if r[0] == ident), None)
            if row is None:
                fail("no line %s (the unit has %s to %s)" % (ident, rows[0][0], rows[-1][0]), "%s next" % tool())
            targets.append((unit, row))
        else:
            targets.extend((unit, r) for r in rows if r[0] not in states)
            if not any(r[0] not in states for r in rows):
                fail("%s has no open line" % unit_id, "%s next" % tool())
    if status in ("finding", "lead") and len(targets) > 3:
        fail("a finding or lead is one root cause: name at most three lines for one record", "%s settle <id> %s" % (tool(), status))
    # One judgment over several lines stands on a place the reviewer read: the control that covers
    # them, as path:line (a package or lockfile for the dependencies unit). Without it a unit could
    # be waved through unread.
    cited = None
    if status == "blocked" and all(u.get("error") for u in {u["id"]: u for u, _ in targets}.values()):
        cited = "tool failure"  # the unit's step failed: the failure is the reason, no line was shown
    elif status in ("safe", "hardening", "blocked") and len(targets) > 1:
        cited = cited_location(state, note, targets)
        if cited is None:
            first = targets[0][1]
            example = ("%s:%d" % (first[1], first[2])) if first[1] and first[2] else (first[1] or "src/app.py:42")
            if targets[0][0]["class"] == "dependencies":
                fail("a judgment of %d packages names the package or lockfile it rests on" % len(targets), "%s settle %s %s \"<reason naming the package>\"" % (tool(), " ".join(ids), status))
            fail("a judgment of %d lines names the place you read that covers them, as path:line in the project (e.g. %s)" % (len(targets), example), "%s settle %s %s \"<reason with path:line>\"" % (tool(), " ".join(ids), status))
    written = []
    previous = state.load(PREVIOUS_FILE, None) or {"issues": []}
    for unit, (hid, rel, number, shape, text) in targets:
        if status == "still" and unit["class"] == "previous":
            issue = previous["issues"][int(hid.split(".")[1]) - 1] if "." in hid and hid.split(".")[1].isdigit() else None
            if issue and issue.get("record"):
                record = dict(issue["record"])
                record["hit"] = hid
                record["unit"] = unit["id"]
                record["carried_from"] = previous.get("archived", "")
                record["recorded"] = time.strftime("%Y-%m-%d %H:%M")
                findings = [f for f in state.findings() if f.get("fingerprint") != record.get("fingerprint")]
                findings.append(record)
                state.save("findings.json", findings)
                states[hid] = {"status": "finding" if record["verdict"] == "confirmed" else "lead", "note": "still open: " + note, "record": "carried"}
            else:
                states[hid] = {"status": "hardening", "note": "still open: " + note}
            continue
        if status in ("finding", "lead"):
            os.makedirs(state.drafts, exist_ok=True)
            n = 1
            while os.path.exists(os.path.join(state.drafts, "finding-%d.json" % n)) or os.path.exists(state.path("finding-%d.json" % n)):
                n += 1
            name = "finding-%d.json" % n
            record = json.loads(json.dumps(TEMPLATES["confirmed" if status == "finding" else "needs_validation"]))
            record["class"] = unit["class"]
            record["unit"] = unit["id"]
            record["hit"] = hid
            record["fingerprint"] = "? %s:%s:<function>" % (unit["class"], rel or "<file>")
            if unit["class"] == "dependencies" and rel:
                package = text.split(":")[0].split(" ")[1] if " " in text.split(":")[0] else text.split(":")[0]
                record["fingerprint"] = ("dependencies:%s:%s" % (rel, package)).lower()
                record["title"] = "? what a lower-trust party can do through %s at the pinned version" % package
                lock_text = read_text(os.path.join(state.root, rel)) or ""
                line_no = next((i for i, l in enumerate(lock_text.split("\n"), 1) if package.split("/")[-1] in l), 1)
                record["trace"][-1] = {"kind": "sink", "file": rel, "line": line_no, "what": "? the package at its pinned version, and the call in this project that reaches the vulnerable code"}
            elif rel and number:
                record["trace"][-1] = {"kind": "sink", "file": rel, "line": number, "what": "? what the value does here: " + text[:80]}
            with open(os.path.join(state.drafts, name), "w", encoding="utf-8") as handle:
                json.dump(record, handle, indent=1)
            written.append((hid, os.path.join(state.drafts_rel, name)))
            states[hid] = {"status": status, "note": note, "record": name}
        else:
            states[hid] = {"status": status, "note": note}
            if cited:
                states[hid]["cited"] = cited
    state.save("hits-state.json", states)
    sync_units(state, coverage)
    coverage = state.coverage()
    coverage["report_stale"] = True
    state.save("coverage.json", coverage)
    closed = []
    for unit in {u["id"]: u for u, _ in targets}.values():
        if unit["status"] in ("covered", "candidate", "blocked"):
            closed.append((unit, state.path(os.path.join("units", unit_report(state, coverage, unit, catalog)))))
    print("Settled %s as %s." % (", ".join(hid for _, (hid, *_) in targets), status))
    if written:
        for hid, path in written:
            print("Record for %s: %s" % (hid, path))
        if status == "finding":
            print("Severity comes from the demonstrated result, never from the class name; overall never exceeds impact:")
            for anchor in SEVERITY_ANCHORS:
                print("  " + anchor)
        print("Next: fill every '? ' value of %s with edit (trace: entrypoint first, the sink is prefilled; a real path:line each), then %s finding %s" % (written[0][1], tool(), written[0][1]))
        return
    still_open = any(r[0] not in states for u in coverage["units"] for r in hit_rows(state, u))
    if closed and still_open:
        # A unit is one piece of the work: its one-line result goes to the user now, and the audit
        # goes on with the next unit from the state on disk (the host continues the turn while the
        # tool says work is open), so each unit starts from a short history.
        for unit, report_path in closed:
            print("%s is settled: %s" % (unit["id"], unit_line(state, unit, catalog, report_path)))
        print("Next: reply to the user with that one line; the audit goes on with the next unit after it.")
    else:
        print("Next: %s next" % tool())


def unit_line(state, unit, catalog, report_path):
    """One line on a settled unit for the user: counts per verdict and where its report is."""
    states = hit_states(state)
    counts = {}
    for hid, *_ in hit_rows(state, unit):
        st = states.get(hid)
        key = st["status"] if st else "open"
        counts[key] = counts.get(key, 0) + 1
    parts = ["%d %s" % (n, VERDICT_WORDS.get(k, k)) for k, n in sorted(counts.items(), key=lambda kv: -kv[1]) if k != "open"]
    return "%s, %d lines reviewed: %s; report %s." % (catalog.by_id[unit["class"]]["name"], sum(counts.values()), ", ".join(parts) or "nothing judged", report_path)


# ----------------------------------------------------------------------------- per-unit reports

VERDICT_WORDS = {"safe": "no vulnerability", "hardening": "hardening recommended", "lead": "needs validation", "finding": "confirmed vulnerability", "blocked": "could not be assessed", "fixed": "fixed since the previous audit", "still": "still open"}


def sentence(text):
    text = str(text or "").strip().rstrip(".")
    return (text[0].upper() + text[1:] + ".") if text else ""


def unit_report(state, coverage, unit, catalog):
    """Write `units/<id>-<class>.md`: the unit's review in full, as a reader would want it: what the
    class is, why it applies here, each finding and lead in prose, the hardening notes, and every
    line reviewed with its verdict and the reviewer's reasoning."""
    cls = catalog.by_id[unit["class"]]
    states = hit_states(state)
    rows = hit_rows(state, unit)
    judged = [(r, states.get(r[0])) for r in rows]
    counts = {}
    for _, st in judged:
        counts[st["status"] if st else "open"] = counts.get(st["status"] if st else "open", 0) + 1
    records = {f.get("hit"): f for f in state.findings() if f.get("hit")}
    deps = (state.load("deps.json", {}) or {}).get("rows") or []
    lines = []
    add = lines.append
    add("# %s: %s" % (unit["id"], cls["name"]))
    add("")
    add("Status: %s. %d lines reviewed%s." % (unit["status"].replace("_", " "), len(rows), "".join(", %d %s" % (n, VERDICT_WORDS.get(k, k)) for k, n in sorted(counts.items()) if k != "open") + (", %d still open" % counts["open"] if counts.get("open") else "")))
    add("")
    add("## Scope")
    add("")
    add("This class applies to the project because of: %s." % unit["reason"])
    if unit.get("error"):
        add("")
        add("The audit tool failed while showing this unit's lines (%s); the lines below were judged from what could be shown, and the rest is marked not assessed." % unit["error"])
    add("")
    add("The property that must hold: %s" % sentence(cls["invariant"]))
    add("")
    add("What would count as a vulnerability here: %s" % sentence(cls["counts"]))
    add("")
    add("What is only a hardening note: %s" % sentence(cls["hardening"]))
    findings = [(r, records[r[0]]) for r, st in judged if st and st["status"] in ("finding", "lead") and r[0] in records]
    if findings:
        add("")
        add("## Findings and leads")
        for (hid, rel, number, shape, text), rec in findings:
            add("")
            if rec["verdict"] == "confirmed":
                add("### %s (%s severity)" % (rec["title"], rec["severity"]["overall"]))
                add("")
                add("Affected code: " + " then ".join("`%s:%d` (%s: %s)" % (s["file"], s["line"], s["kind"], s["what"]) for s in rec["trace"]) + ".")
                add("")
                add("Who can exploit it: %s" % sentence(rec["principal"]))
                add("")
                add("Why the existing controls do not stop it: %s" % sentence(rec["control"]))
                add("")
                add("Impact: %s" % sentence(rec["impact"]))
                if rec.get("conditions"):
                    add("")
                    add("Conditions: " + "; ".join(rec["conditions"]) + ".")
                add("")
                add("Evidence: %s Observed: %s" % (sentence(rec["evidence"]["how"]), sentence(rec["evidence"]["observed"])))
                add("")
                add("Severity: likelihood %s, impact %s, overall %s." % (rec["severity"]["likelihood"], rec["severity"]["impact"], rec["severity"]["overall"]))
                add("")
                add("Recommended fix: in %s, %s Regression test: %s" % (rec["fix"]["where"], sentence(rec["fix"]["change"]), sentence(rec["fix"]["test"])))
            else:
                add("### %s (lead, needs validation)" % rec["title"])
                add("")
                add("Affected code: " + " then ".join("`%s:%d` (%s: %s)" % (s["file"], s["line"], s["kind"], s["what"]) for s in rec["trace"]) + ".")
                add("")
                add("Who could exploit it: %s" % sentence(rec["principal"]))
                add("")
                add("The one fact outside the code that decides: %s" % sentence(rec["blocker"]))
                add("")
                add("How to settle it without attacking anything: %s" % sentence(rec["check"]))
    hardening = [(r, st) for r, st in judged if st and st["status"] == "hardening"]
    if hardening:
        add("")
        add("## Hardening recommendations")
        add("")
        add("No reachable attack was found behind these lines, but each names a change worth making.")
        add("")
        seen = set()
        for (hid, rel, number, shape, text), st in hardening:
            where = ("`%s:%d`" % (rel, number)) if number else ("`%s`" % rel if rel else "")
            key = (st["note"], rel)
            if key in seen:
                continue
            seen.add(key)
            add("- %s %s" % (where, sentence(st["note"])))
    blocked = [(r, st) for r, st in judged if st and st["status"] == "blocked"]
    if blocked:
        add("")
        add("## Not assessed")
        add("")
        for (hid, rel, number, shape, text), st in blocked:
            add("- `%s%s`: %s" % (rel, ":%d" % number if number else "", sentence(st["note"])))
    add("")
    add("## Line-by-line review")
    add("")
    if unit["class"] == "dependencies":
        groups = {}
        for row in deps:
            groups.setdefault((row["eco"], row["name"], row["version"], row["lockfile"]), []).append(row)
        for (hid, rel, number, shape, text), st in judged:
            name = text.split(":")[0]
            add("### %s %s" % (hid, name))
            add("")
            key = next((k for k in groups if "%s %s %s" % (k[0], k[1], k[2]) == name), None)
            if key:
                for row in groups[key]:
                    ids = row["id"] + (" / " + row["aliases"][0] if row["aliases"] else "")
                    add("- %s, %s: %s Fixed in %s." % (ids, row["severity"], sentence(row["summary"]), ", ".join(sorted(set(row["fixed"]))[:3]) or "no version listed"))
                add("")
            add("Verdict: %s. %s" % (VERDICT_WORDS.get(st["status"], st["status"]) if st else "open", sentence(st["note"]) if st and st.get("note") else ""))
            add("")
    else:
        by_file = {}
        for (hid, rel, number, shape, text), st in judged:
            by_file.setdefault(rel or "(the unit as a whole)", []).append(((hid, rel, number, shape, text), st))
        for rel, items in by_file.items():
            add("### %s" % rel)
            add("")
            for (hid, _, number, shape, text), st in items:
                where = ("line %d" % number) if number else "the unit"
                verdict = VERDICT_WORDS.get(st["status"], st["status"]) if st else "not yet reviewed"
                add("- %s, %s, flagged as %s: `%s`" % (hid, where, shape, text.replace("`", "'")[:140]))
                add("  Verdict: %s.%s" % (verdict, (" " + sentence(st["note"])) if st and st.get("note") else ""))
            add("")
    os.makedirs(state.path("units"), exist_ok=True)
    name = "%s-%s.md" % (unit["id"], unit["class"])
    with open(state.path(os.path.join("units", name)), "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines).rstrip() + "\n")
    return name


# ----------------------------------------------------------------------------- previous audits

PREVIOUS_FILE = "previous.json"


def archive_previous(state):
    """A new audit on a project with a finished one: keep the old run under runs/<stamp>/ and
    return its issues (findings, leads, hardening notes) for the re-check, or None when there is
    no previous run."""
    if not os.path.isfile(state.path("REPORT.md")) and not os.path.isfile(state.path("hits-state.json")):
        return None
    findings = state.findings()
    states = hit_states(state)
    coverage = state.load("coverage.json", {}) or {}
    issues = []
    for record in findings:
        if record.get("verdict") not in ("confirmed", "needs_validation"):
            continue
        sink = record["trace"][-1]
        issues.append({"kind": "finding" if record["verdict"] == "confirmed" else "lead", "title": record["title"], "file": sink["file"], "line": sink["line"], "text": sink.get("text", ""), "severity": (record.get("severity") or {}).get("overall", ""), "record": record, "class": record.get("class", "")})
    hits_by_unit = {}
    for unit in coverage.get("units", []):
        for hid, rel, number, shape, text in hit_rows(state, unit):
            hits_by_unit[hid] = (unit, rel, number, shape, text)
    for hid, st in states.items():
        if st.get("status") != "hardening" or hid not in hits_by_unit:
            continue
        unit, rel, number, shape, text = hits_by_unit[hid]
        issue = {"kind": "hardening", "title": st.get("note", ""), "file": rel, "line": number, "text": text, "class": unit["class"], "hit": hid}
        if unit["class"] == "dependencies":
            parts = text.split(":")[0].split(" ")
            if len(parts) >= 3:
                issue["package"] = {"eco": parts[0], "name": parts[1], "version": parts[2]}
        issues.append(issue)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    archive = state.path(os.path.join("runs", stamp))
    os.makedirs(archive, exist_ok=True)
    for name in ("REPORT.md", "findings.json", "hits-state.json", "coverage.json", "recon.json", "deps.json"):
        if os.path.isfile(state.path(name)):
            os.replace(state.path(name), os.path.join(archive, name))
    for folder in ("units", "hits"):
        if os.path.isdir(state.path(folder)):
            os.replace(state.path(folder), os.path.join(archive, folder))
    # The old run's record drafts (normally gone with the project's tmp/) must not block the new one.
    if os.path.isdir(state.drafts):
        for name in os.listdir(state.drafts):
            if re.match(r"finding-\d+\.json$", name):
                os.replace(os.path.join(state.drafts, name), os.path.join(archive, name))
    previous = {"archived": stamp, "report": os.path.join("runs", stamp, "REPORT.md"), "issues": issues}
    state.save(PREVIOUS_FILE, previous)
    return previous


def recheck_previous(state, previous, packages_now):
    """What the code says about each previous issue now: the flagged line still present, moved or
    gone; a flagged package still pinned at a vulnerable version, upgraded, or removed."""
    for issue in previous["issues"]:
        if issue.get("package"):
            pkg = issue["package"]
            current = next((v for (eco, name, v) in packages_now if eco == pkg["eco"] and name == pkg["name"]), None)
            if current is None:
                issue["now"] = "the package is no longer pinned in a lockfile"
            elif current != pkg["version"]:
                issue["now"] = "the package is now pinned at %s (was %s)" % (current, pkg["version"])
            else:
                issue["now"] = "still pinned at %s" % pkg["version"]
            continue
        rel = issue.get("file")
        if not rel:
            issue["now"] = "no line to compare"
            continue
        full = os.path.join(state.root, rel)
        if not os.path.isfile(full):
            issue["now"] = "the file %s no longer exists" % rel
            continue
        text = (issue.get("text") or "").strip()
        content = read_text(full) or ""
        lines = content.split("\n")
        if text and issue.get("line") and 0 < issue["line"] <= len(lines) and lines[issue["line"] - 1].strip() == text:
            issue["now"] = "the flagged line is unchanged at %s:%d" % (rel, issue["line"])
        elif text and any(l.strip() == text for l in lines):
            issue["now"] = "the flagged line still exists in %s, at another line" % rel
        elif text:
            issue["now"] = "the flagged line is gone from %s (changed or removed)" % rel
        else:
            issue["now"] = "the file still exists; compare %s:%d by hand" % (rel, issue.get("line") or 0)
    state.save(PREVIOUS_FILE, previous)


def previous_rows(state, unit):
    """The U0 lines: one per previous issue."""
    previous = state.load(PREVIOUS_FILE, None)
    if not previous:
        return []
    out = []
    for index, issue in enumerate(previous["issues"], 1):
        where = ("%s:%d" % (issue["file"], issue["line"])) if issue.get("file") and issue.get("line") else (issue.get("file") or "")
        label = {"finding": "confirmed finding, %s" % issue.get("severity", ""), "lead": "lead (needs validation)", "hardening": "hardening note"}[issue["kind"]]
        text = "%s: %s. Now: %s" % (label, issue["title"][:120], issue.get("now", "not compared"))
        out.append(("%s.%d" % (unit["id"], index), issue.get("file") or "", issue.get("line") or 0, "previous " + issue["kind"], text))
    return out


# ----------------------------------------------------------------------------- main

COMMANDS = {"next": cmd_next, "settle": cmd_settle, "recon": cmd_recon, "hunt": cmd_hunt, "cover": cmd_cover, "new": cmd_new, "finding": cmd_finding, "status": cmd_status, "report": cmd_report, "scan": cmd_scan, "deps": cmd_deps, "cve": cmd_cve}


def main(argv):
    if not argv or argv[0] in ("-h", "--help", "help"):
        print(__doc__.strip())
        return 0
    command = COMMANDS.get(argv[0])
    if command is None:
        fail("unknown command %s" % argv[0], "one of: " + ", ".join(COMMANDS))
    command(argv[1:])
    return 0


if __name__ == "__main__":
    # A result piped into head must not end in a traceback.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    sys.exit(main(sys.argv[1:]))
