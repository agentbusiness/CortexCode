#!/usr/bin/env python3
"""secaudit: the mechanical half of a source-code security audit, for CortexCode's model.

    python3 secaudit.py next                       the one driving command: maps the project on the first call, then the next lines to judge, then the report
    python3 secaudit.py settle <id>... safe|hardening|lead|finding|blocked ["note"]
    python3 secaudit.py recon [--scope <path>...]
    python3 secaudit.py hunt <unit> [--all]
    python3 secaudit.py cover <unit> covered|candidate|blocked ["<note>"]
    python3 secaudit.py new confirmed|needs_validation|rejected
    python3 secaudit.py finding <file.json>
    python3 secaudit.py status [--fixing | --finding <id> | --unit <id>]   (exit 0 once the work is done)
    python3 secaudit.py show <id>                   one finding from the project's graph, for a fix session
    python3 secaudit.py fix-plan <id>               the exact upgrade commands of a dependency finding
    python3 secaudit.py recheck [<id>]              what the code says now; the host moves what it sees fixed
    python3 secaudit.py resolve <id> "<how>"        record a fix (the host checks the sink is gone)
    python3 secaudit.py report [--partial]
    python3 secaudit.py scan [<path>...] [--changed [<ref>]]
    python3 secaudit.py scan --json <abs path>... [--classes a,b]      one JSON object, nothing written
    python3 secaudit.py deps [--json] [--root <dir>]                   --json: one JSON object, nothing written
    python3 secaudit.py check-change [--root <dir>]                    the working tree's own changes: silent when clean, exit 3 with findings
    python3 secaudit.py watch [--root <dir>]                           the dependency watch: new advisories since the last one
    python3 secaudit.py cve <GHSA-... | CVE-...>

Runs from the repository root (every CortexCode `run` starts there). The audit's records (its map,
units, the lines judged, the vulnerable dependencies and the findings) live in the project's graph,
kept by the memory plugin and reached through `cx call memory.security.*`; without CortexCode the
audit cannot be kept and the commands that record say so. The reports (REPORT.md, units/*.md) are
files under `$CORTEX_HOME/audits/`; the finding drafts the model fills are in the project's
`tmp/secaudit/`. Python 3.8+, standard library only. The attack-class catalog is `classes-*.json`
beside this file: which classes apply to a project is decided from structural facts (languages,
imports, files, code shapes), and each class carries the invariant, where to look, what counts as a
finding, and the code shapes to search for. The model does the tracing and the judging; this tool
does the rest.

Live sources, queried at call time, never mirrored (each answers within seconds or is skipped with
one line; a project with a `.cortex-offline` file is never looked up):
- OSV (api.osv.dev): known vulnerabilities for the exact dependency versions in the project's
  lockfiles (npm, PyPI, crates.io, Go, RubyGems, Packagist, Maven, NuGet), with the fixed versions.
- CISA's Known Exploited Vulnerabilities catalog and FIRST EPSS: which of those are exploited in
  the wild, and how likely exploitation is; they weigh the priority.
- NVD (services.nvd.nist.gov): a CVE's description, CVSS and CWE ids when OSV has no record.
- The CWE API (cwe-api.mitre.org): the mitigations of a weakness class, for `cve`.
The code shapes searched for (`classes-*.json` beside this file) are this tool's own heuristics,
reference text shipped with it; a hit is a question to trace, never a finding.
"""

import concurrent.futures
import fnmatch
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
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
KEV_URL = "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json"
EPSS_URL = "https://api.first.org/data/v1/epss?cve="
FETCH_TIMEOUT = 8
FETCH_MAX_BYTES = 4_000_000
KEV_MAX_BYTES = 12_000_000
OSV_BATCH = 1000
OSV_WORKERS = 8
EPSS_BATCH = 50
DEPS_SHOWN = 40
RECON_DEPS_SHOWN = 8
HITS_WRITE_BATCH = 2000
CHANGE_LINES_SHOWN = 40

SKIP_DIRS = {".git", "node_modules", "target", "dist", "build", "out", ".next", ".nuxt", ".svelte-kit", "vendor", "venv", ".venv", "env", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox", ".idea", ".vscode", "coverage", "tmp", ".cache", "Pods", "DerivedData", ".gradle", ".terraform", "bower_components", ".yarn", ".pnpm-store", "site-packages", ".dart_tool", "bin", "obj"}
SKIP_FILE_GLOBS = ["*.min.js", "*.min.css", "*.map", "*.lock", "*.lockb", "*.lockfile", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "Cargo.lock", "go.sum", "poetry.lock", "Pipfile.lock", "composer.lock", "Gemfile.lock", "*.snap", "*.svg", "*.csv", "*.tsv", "*.jsonl", "*.ndjson", "*.pb.go", "*_pb2.py", "*.d.ts", "*.generated.*", "*.gen.go"]
LOCKFILES = ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lock", "bun.lockb", "Cargo.lock", "go.sum", "poetry.lock", "Pipfile.lock", "uv.lock", "pdm.lock", "requirements.txt", "composer.lock", "Gemfile.lock", "packages.lock.json", "gradle.lockfile", "flake.lock"]
MANIFESTS = {"package.json": "npm", "requirements.txt": "pip", "requirements-dev.txt": "pip", "pyproject.toml": "python", "setup.py": "python", "Pipfile": "pipenv", "Cargo.toml": "cargo", "go.mod": "go", "Gemfile": "bundler", "composer.json": "composer", "pom.xml": "maven", "build.gradle": "gradle", "build.gradle.kts": "gradle", "*.csproj": "nuget", "Package.swift": "swiftpm", "pubspec.yaml": "pub", "mix.exs": "mix"}
MANIFEST_LOCK = {"package.json": ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lock", "bun.lockb"], "pyproject.toml": ["poetry.lock", "uv.lock", "pdm.lock", "requirements.txt"], "Pipfile": ["Pipfile.lock"], "Cargo.toml": ["Cargo.lock"], "go.mod": ["go.sum"], "Gemfile": ["Gemfile.lock"], "composer.json": ["composer.lock"], "build.gradle": ["gradle.lockfile"], "build.gradle.kts": ["gradle.lockfile"]}
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
# What the graph records as the audit's tool version.
SKILL_VERSION = "secaudit 2026-11-01"

# Strong host signals: a hit of one of these shapes is a real credential format, never waved off.
STRONG_PATTERNS = {"private key block", "AWS access key id", "GitHub token", "Slack token", "Stripe live key"}
SIGNAL_SEVERITY = {"key": "high", "history": "high", "malicious": "critical", "kev": "high"}
TEST_PLACE = re.compile(r"(^|/)(tests?|spec|specs|__tests__|__mocks__|fixtures?|testdata|test-data|examples?|samples?|mocks?)(/|$)|(^|/)(test_[^/]*|[^/]*_test\.[^/]*|[^/]*\.(test|spec)\.[^/]*)$")
EXAMPLE_VALUE = re.compile(r"(?i)example|dummy|fake|placeholder|changeme|not[-_]?a[-_]?real|redacted|xxxx|0000|1234|abcd|test")


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
    the root is a repository, else a walk; without ignored, generated, huge or binary files. The
    lockfiles and the AI-agent files are listed apart (`repo_files.lockfiles`, `.agent_files`),
    also from folders the scan skips."""
    listed = git(["ls-files", "-co", "--exclude-standard", "-z"], root)
    if listed is not None:
        rels = [rel for rel in listed.split("\0") if rel]
    else:
        rels = []
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and (not d.startswith(".") or d in AGENT_DIRS)]
            for name in filenames:
                rels.append(os.path.relpath(os.path.join(dirpath, name), root))
    scope = [s.rstrip("/") for s in scope]
    files = []
    repo_files.lockfiles = []
    repo_files.agent_files = []
    for rel in sorted(set(rels)):
        if scope and not any(rel == s or rel.startswith(s + "/") for s in scope):
            continue
        base = os.path.basename(rel)
        parents = rel.split("/")[:-1]
        if (base in LOCKFILES or base in ("go.mod", "pom.xml") or (base.startswith("requirements") and base.endswith(".txt"))) and not any(part in SKIP_DIRS for part in parents):
            repo_files.lockfiles.append(rel)
        if agent_file_kind(rel) and not any(part in SKIP_DIRS - {".vscode"} for part in parents) and os.path.isfile(os.path.join(root, rel)):
            repo_files.agent_files.append(rel)
        if skipped(rel):
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


def normalize(line):
    """A line as the evidence compares it: ASCII whitespace runs collapsed to one space, trimmed
    (the memory plugin normalizes the same way)."""
    return re.sub(r"[ \t\n\r\x0c]+", " ", line).strip(" ")


def line_hash(line):
    normal = normalize(line)
    if len(normal) < 3:
        return None
    return hashlib.sha256(normal.encode("utf-8")).hexdigest()


# ----------------------------------------------------------------------------- secrets in excerpts

SECRET_VALUE_RES = [
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{22,}\b"),
    re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"),
    re.compile(r"\b[sr]k_live_[A-Za-z0-9]{16,}\b"),
    re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b"),
    re.compile(r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    re.compile(r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}\b"),
    re.compile(r"\bglpat-[A-Za-z0-9_-]{20,}\b"),
    re.compile(r"\bnpm_[A-Za-z0-9]{36}\b"),
]
QUOTED_SECRET_RE = re.compile(r"""(?i)(\b(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key|token)\b["']?\s*[:=]\s*["'])([^"'\s]{4,})(["'])""")
ENV_SECRET_RE = re.compile(r"""(?i)^(\s*(?:export\s+)?[A-Z0-9_]*(?:PASSWORD|PASSWD|SECRET|TOKEN|API_KEY|APIKEY|ACCESS_KEY|PRIVATE_KEY|AUTH)[A-Z0-9_]*\s*=\s*["']?)([^"'\s#]{4,})""")
URL_PASSWORD_RE = re.compile(r"(\b[a-z][a-z0-9+.-]*://[^:/\s@]+:)([^@\s/]{3,})(@)")
BEARER_RE = re.compile(r"(?i)(\b(?:Bearer|Basic)\s+)([A-Za-z0-9._~+/=-]{8,})")
SECRET_NAME_RE = re.compile(r"(?i)(password|passwd|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|credential|auth)")


def masked(value):
    """A secret as shown: its first four characters and an ellipsis."""
    return value[:4] + "\u2026"


def is_reference(value):
    """A value that names a secret kept elsewhere (`${VAR}`, `$VAR`, `{env:...}`, `<your key>`)."""
    value = value.strip()
    return value.startswith(("$", "{{", "{env", "env:", "%", "<", "#{", "process.env", "os.environ")) or value.lower() in ("", "none", "null", "changeme", "xxx")


def mask(text):
    """`text` with every secret value it carries cut to its first four characters."""
    text = str(text or "")
    for regex in SECRET_VALUE_RES:
        text = regex.sub(lambda m: masked(m.group(0)), text)
    text = QUOTED_SECRET_RE.sub(lambda m: m.group(1) + (m.group(2) if is_reference(m.group(2)) or m.group(2).endswith("\u2026") else masked(m.group(2))) + m.group(3), text)
    text = ENV_SECRET_RE.sub(lambda m: m.group(1) + (m.group(2) if is_reference(m.group(2)) or m.group(2).endswith("\u2026") else masked(m.group(2))), text)
    text = URL_PASSWORD_RE.sub(lambda m: m.group(1) + (m.group(2) if is_reference(m.group(2)) else masked(m.group(2))) + m.group(3), text)
    text = BEARER_RE.sub(lambda m: m.group(1) + (m.group(2) if is_reference(m.group(2)) or m.group(2).endswith("\u2026") else masked(m.group(2))), text)
    return text


def shown(text, limit=160):
    """An excerpt as printed and stored: secrets masked, invisible and reordering characters
    dropped, whitespace collapsed, at most `limit` characters."""
    visible = "".join(ch for ch in str(text or "") if ch not in ZERO_WIDTH and ch not in BIDI and not 0xE0000 <= ord(ch) <= 0xE007F)
    return mask(normalize(visible))[:limit]


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
    "id": "previous", "name": "Issues already known", "applies": {}, "patterns": [],
    "invariant": "Every issue already known to the project is either fixed in the current code or still open and carried into this report.",
    "look": [
        "A confirmed finding or lead: read the flagged line and its callers again; the host's re-check says whether the line is unchanged, moved or gone, which is a hint, not a verdict.",
        "A hardening note: is the change made (an upgrade, a moved secret, a tightened check)?",
        "A dependency finding: the re-check compares the pinned version with the one recorded."
    ],
    "counts": "A known finding whose path still exists unchanged is still a finding: settle it as still. A known issue that the code no longer has is fixed.",
    "hardening": "A known hardening note not acted on stays a hardening note (still).",
}


class Catalog:
    def __init__(self, directory=None):
        self.directory = directory or str(HERE)
        self.classes = load_catalog(self.directory)
        self.by_id = {cls["id"]: cls for cls in self.classes}
        self.by_id["previous"] = PREVIOUS_CLASS

    def patterns_for(self, lang, markers=True):
        """(class id, pattern) pairs that apply to files of `lang`; without the place markers (a
        handler, a registration: where to read, not a defect) when `markers` is false."""
        out = []
        for cls in self.classes:
            for pattern in cls.get("patterns", []):
                if not markers and pattern.get("kind") == "marker":
                    continue
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

def strip_jsonc(text):
    """JSON with comments and trailing commas (bun.lock, editor configs) as plain JSON."""
    out, i, n, in_string = [], 0, len(text), False
    while i < n:
        ch = text[i]
        if in_string:
            out.append(ch)
            if ch == "\\" and i + 1 < n:
                out.append(text[i + 1])
                i += 2
                continue
            if ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
            out.append(ch)
            i += 1
        elif text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            i = n if end < 0 else end + 2
        else:
            out.append(ch)
            i += 1
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out))


def parse_lockfile(rel, text):
    """(ecosystem, name, version) for every package a lockfile pins; [] for a file that is not one.
    Raises ValueError for a lockfile that cannot be read (the caller lists it as not parsed)."""
    name = os.path.basename(rel)
    out = []
    if name == "package-lock.json":
        data = json.loads(text)
        for path, info in (data.get("packages") or {}).items():
            if path and isinstance(info, dict) and info.get("version") and "node_modules/" in path and not info.get("link"):
                out.append(("npm", path.rsplit("node_modules/", 1)[1], info["version"]))

        def walk(deps):
            for pkg, info in (deps or {}).items():
                if isinstance(info, dict) and info.get("version"):
                    out.append(("npm", pkg, info["version"]))
                    walk(info.get("dependencies"))
        if not data.get("packages"):
            walk(data.get("dependencies"))
    elif name == "bun.lock":
        data = json.loads(strip_jsonc(text))
        for key, entry in (data.get("packages") or {}).items():
            spec = entry[0] if isinstance(entry, list) and entry and isinstance(entry[0], str) else None
            if not spec or "@" not in spec[1:]:
                continue
            pkg, version = spec.rsplit("@", 1)
            if re.match(r"^\d", version):
                out.append(("npm", pkg, version))
    elif name == "yarn.lock":
        for match in re.finditer(r'^"?((?:@[^/\s"]+/)?[^@\s"]+)@[^\n]*:\n(?:[^\n]*\n)*?\s+version:?\s+"?([^"\s]+)"?', text, re.M):
            out.append(("npm", match.group(1), match.group(2)))
    elif name == "pnpm-lock.yaml":
        section = text.split("\npackages:", 1)
        if len(section) == 2:
            for match in re.finditer(r"^  '?/?((?:@[^/\s']+/)?[^@\s'/]+)@([^\s'(:]+)", section[1], re.M):
                out.append(("npm", match.group(1), match.group(2)))
    elif name.startswith("requirements") and name.endswith(".txt"):
        for match in re.finditer(r"^\s*([A-Za-z0-9][A-Za-z0-9_.-]*)\s*(?:\[[^\]]*\])?\s*(===|==)\s*([A-Za-z0-9_.+!-]+)\s*(?:;|#|\\|$)", text, re.M):
            if "*" not in match.group(3):
                out.append(("PyPI", match.group(1), match.group(3)))
        # A direct reference to a built file names its exact version: `pkg @ https://.../pkg-1.2.3-py3-none-any.whl`.
        for match in re.finditer(r"^\s*([A-Za-z0-9][A-Za-z0-9_.-]*)\s*@\s*\S*/([A-Za-z0-9_.]+)-(\d[A-Za-z0-9_.+!]*?)(?:-[^/\s]*)?\.(?:whl|tar\.gz|zip)\b", text, re.M):
            out.append(("PyPI", match.group(1), match.group(3)))
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
    elif name == "go.sum":
        # The modules whose source was downloaded (a line without /go.mod), at their highest version.
        best = {}
        for match in re.finditer(r"^(\S+)\s+(v[^\s/]+)\s+h1:", text, re.M):
            module, version = match.group(1), match.group(2)
            if module not in best or version_key(version.lstrip("v")) > version_key(best[module].lstrip("v")):
                best[module] = version
        out.extend(("Go", module, version) for module, version in sorted(best.items()))
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
    elif name == "gradle.lockfile":
        for match in re.finditer(r"^([\w.-]+):([\w.-]+):([\w.+-]+)=", text, re.M):
            out.append(("Maven", match.group(1) + ":" + match.group(2), match.group(3)))
    elif name == "pom.xml":
        for block in re.findall(r"<dependency>(.*?)</dependency>", text, re.S):
            group = re.search(r"<groupId>\s*([^<\s]+)\s*</groupId>", block)
            artifact = re.search(r"<artifactId>\s*([^<\s]+)\s*</artifactId>", block)
            version = re.search(r"<version>\s*([^<\s$]+)\s*</version>", block)
            if group and artifact and version:
                out.append(("Maven", group.group(1) + ":" + artifact.group(1), version.group(1)))
    seen, unique = set(), []
    for item in out:
        if item not in seen:
            seen.add(item)
            unique.append(item)
    return unique


def manifest_pins(rel, text):
    """Exact versions a manifest names itself (package.json `"x": "1.2.3"`): what a change check
    can look up when no lockfile moved with it."""
    if os.path.basename(rel) != "package.json":
        return []
    try:
        data = json.loads(text)
    except ValueError:
        return []
    out = []
    for section in ("dependencies", "devDependencies", "optionalDependencies", "peerDependencies"):
        for pkg, spec in (data.get(section) or {}).items():
            if isinstance(spec, str) and re.fullmatch(r"=?v?\d+\.\d+\.\d+(?:[-+][\w.-]+)?", spec.strip()):
                out.append(("npm", pkg, spec.strip().lstrip("=v")))
    return out


def project_packages(root, lockfiles=None):
    """Every pinned package of the project's lockfiles: ({(ecosystem, name, version): lockfile},
    [`<lockfile>: <why it was not read>`])."""
    packages, not_parsed, go_modules = {}, [], set()
    for rel in sorted(set(lockfiles if lockfiles is not None else getattr(repo_files, "lockfiles", []))):
        if os.path.basename(rel) == "bun.lockb":
            not_parsed.append("%s: binary lockfile, run `bun install --save-text-lockfile`" % rel)
            continue
        text = read_text(os.path.join(root, rel))
        if text is None:
            not_parsed.append("%s: not readable as text" % rel)
            continue
        try:
            items = parse_lockfile(rel, text)
        except (ValueError, AttributeError, TypeError, IndexError) as error:
            not_parsed.append("%s: %s" % (rel, str(error)[:120]))
            continue
        for item in items:
            # go.sum adds the modules go.mod leaves out; go.mod (read first) wins for the ones it names.
            if os.path.basename(rel) == "go.sum" and (os.path.dirname(rel), item[1]) in go_modules:
                continue
            if os.path.basename(rel) == "go.mod":
                go_modules.add((os.path.dirname(rel), item[1]))
            packages.setdefault(item, rel)
    return packages, not_parsed


# ----------------------------------------------------------------------------- live: OSV, KEV, EPSS, NVD, CWE

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
    """OSV's known vulnerabilities for `packages` ({(eco, name, version): lockfile}), every package
    looked up: a list of rows {eco, name, version, lockfile, id, aliases, severity, summary, fixed,
    cwes, kev, epss, epss_percentile}, worst first. Raises on network trouble."""
    items = list(packages.items())
    flagged = {}
    for start in range(0, len(items), OSV_BATCH):
        chunk = items[start:start + OSV_BATCH]
        queries = [{"package": {"name": name, "ecosystem": eco}, "version": version} for (eco, name, version), _ in chunk]
        result = fetch_json(OSV_URL + "querybatch", {"queries": queries})
        for (key, lockfile), answer in zip(chunk, result.get("results") or []):
            ids = [v["id"] for v in answer.get("vulns") or [] if v.get("id")]
            token = answer.get("next_page_token")
            while token:
                eco, name, version = key
                page = fetch_json(OSV_URL + "query", {"package": {"name": name, "ecosystem": eco}, "version": version, "page_token": token})
                ids.extend(v["id"] for v in page.get("vulns") or [] if v.get("id"))
                token = page.get("next_page_token")
            if ids:
                flagged[(key, lockfile)] = ids
    unique_ids = sorted({vid for ids in flagged.values() for vid in ids})
    records = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=OSV_WORKERS) as pool:
        for vid, record in zip(unique_ids, pool.map(lambda vid: fetch_json(OSV_URL + "vulns/" + urllib.parse.quote(vid)), unique_ids)):
            records[vid] = record
    rows = []
    for ((eco, name, version), lockfile), ids in flagged.items():
        for vid in ids:
            vuln = records.get(vid) or {}
            if vuln.get("withdrawn"):
                continue
            aliases = [a for a in vuln.get("aliases") or [] if a.startswith("CVE-")]
            summary = (vuln.get("summary") or (vuln.get("details") or "").split("\n")[0]).strip()
            rows.append({"eco": eco, "name": name, "version": version, "lockfile": lockfile, "id": vid, "aliases": aliases, "severity": severity_of(vuln), "summary": summary[:120], "fixed": fixed_versions(vuln, name), "cwes": (vuln.get("database_specific") or {}).get("cwe_ids") or [], "kev": False, "epss": None, "epss_percentile": None})
    rows.sort(key=lambda r: (SEVERITY_RANK.get(r["severity"], 5), 0 if r["id"].startswith("GHSA-") else 1, r["name"], r["id"]))
    unique, seen = [], set()
    for row in rows:
        keys = {(row["eco"], row["name"], row["version"], alias) for alias in [row["id"]] + row["aliases"]}
        if keys & seen:
            continue
        seen |= keys
        unique.append(row)
    return unique


def exploit_signals(rows):
    """Mark rows exploited in the wild (CISA KEV) and with their EPSS score; returns one line per
    source that could not be read, or [] when both answered."""
    cves = sorted({alias for row in rows for alias in row["aliases"] + ([row["id"]] if row["id"].startswith("CVE-") else [])})
    if not cves:
        return []
    problems = []
    try:
        request = urllib.request.Request(KEV_URL, headers={"User-Agent": "secaudit (CortexCode)", "Accept": "application/json"})
        with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT * 2) as response:
            payload = response.read(KEV_MAX_BYTES + 1)
        if len(payload) > KEV_MAX_BYTES:
            raise ValueError("the KEV catalog is larger than %d bytes" % KEV_MAX_BYTES)
        known = {item.get("cveID") for item in json.loads(payload.decode("utf-8")).get("vulnerabilities") or []}
        for row in rows:
            row["kev"] = any(c in known for c in row["aliases"] + [row["id"]])
    except LIVE_ERRORS as error:
        problems.append("CISA KEV could not be read (%s)" % (getattr(error, "reason", None) or error))
    scores = {}
    try:
        for start in range(0, len(cves), EPSS_BATCH):
            answer = fetch_json(EPSS_URL + ",".join(cves[start:start + EPSS_BATCH]))
            for item in answer.get("data") or []:
                try:
                    scores[item["cve"]] = (float(item["epss"]), float(item.get("percentile") or 0))
                except (KeyError, TypeError, ValueError):
                    continue
    except LIVE_ERRORS as error:
        problems.append("FIRST EPSS could not be read (%s)" % (getattr(error, "reason", None) or error))
    for row in rows:
        best = max((scores[c] for c in row["aliases"] + [row["id"]] if c in scores), default=None)
        if best:
            row["epss"], row["epss_percentile"] = round(best[0], 5), round(best[1], 5)
    return problems


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


def group_rows(rows):
    """Advisory rows by package: [((eco, name, version, lockfile), [rows])], worst package first."""
    groups = {}
    for row in rows:
        groups.setdefault((row["eco"], row["name"], row["version"], row["lockfile"]), []).append(row)
    return sorted(groups.items(), key=lambda kv: (min(SEVERITY_RANK.get(r["severity"], 5) for r in kv[1]), 0 if any(r.get("kev") for r in kv[1]) else 1, kv[0][1]))


def exploit_words(group):
    words = []
    if any(r.get("kev") for r in group):
        words.append("exploited in the wild (CISA KEV)")
    epss = max((r.get("epss") or 0 for r in group), default=0)
    if epss >= 0.01:
        words.append("EPSS %d%%" % round(epss * 100))
    return words


def deps_lines(rows, packages, not_parsed, shown=DEPS_SHOWN):
    """The dependency check as the model reads it."""
    lines = ["Not read: %s." % item for item in not_parsed]
    if not packages:
        return lines + ["No lockfile pins a dependency here (nothing for OSV to check)."]
    ecos = {}
    for (eco, _, _) in packages:
        ecos[eco] = ecos.get(eco, 0) + 1
    files = sorted(set(packages.values()))
    lines.append("%d pinned packages from %s (%s)." % (len(packages), ", ".join(files[:6]) + (" ..." if len(files) > 6 else ""), ", ".join("%s %d" % kv for kv in sorted(ecos.items()))))
    if not rows:
        lines.append("OSV knows no vulnerability for these exact versions (checked live).")
        return lines
    ordered = group_rows(rows)
    lines.append("Known vulnerabilities (OSV, live): %d advisories in %d packages." % (len(rows), len(ordered)))
    for (eco, name, version, lockfile), group in ordered[:shown]:
        fix = covering_fix(group)
        extra = exploit_words(group)
        lines.append("  %s %s %s (%s): %d advisor%s, worst %s%s; %s" % (eco, name, version, lockfile, len(group), "y" if len(group) == 1 else "ies", group[0]["severity"], (", " + ", ".join(extra)) if extra else "", ("upgrade to %s fixes every one listed" % fix) if fix else "no fixed version listed"))
        for row in group[:3]:
            ids = row["id"] + (" " + row["aliases"][0] if row["aliases"] else "")
            lines.append("    %s %s: %s" % (ids, row["severity"], row["summary"]))
        if len(group) > 3:
            lines.append("    ... %d more advisories" % (len(group) - 3))
    if len(ordered) > shown:
        lines.append("  ... %d more packages (the dependencies unit shows each as a line to judge)" % (len(ordered) - shown))
    return lines


def live_deps(root, lockfiles=None, exploits=True):
    """The OSV check, with KEV and EPSS when `exploits`: (rows, packages, not_parsed, why it did
    not run or None, problems of the exploit sources)."""
    packages, not_parsed = project_packages(root, lockfiles)
    if not packages:
        return [], packages, not_parsed, None, []
    if offline_here(root):
        return [], packages, not_parsed, "offline project: the live dependency check (OSV) did not run; %d pinned packages unchecked" % len(packages), []
    try:
        rows = osv_check(packages)
    except LIVE_ERRORS as error:
        return [], packages, not_parsed, "the live dependency check (OSV) could not run (%s); %d pinned packages unchecked" % (getattr(error, "reason", None) or error, len(packages)), []
    return rows, packages, not_parsed, None, exploit_signals(rows) if exploits else []


def advisory_records(rows):
    """The graph's advisories from OSV rows."""
    return [{"ecosystem": r["eco"], "package": r["name"], "version": r["version"], "lockfile": r["lockfile"], "advisory": r["id"], "aliases": r["aliases"], "severity": r["severity"], "summary": r["summary"], "fixed": sorted(set(r["fixed"])), "cwes": r["cwes"], "kev": bool(r.get("kev")), "epss": r.get("epss"), "epss_percentile": r.get("epss_percentile")} for r in rows]


def rows_of(advisories):
    """OSV rows from the graph's advisories."""
    return [{"eco": a["ecosystem"], "name": a["package"], "version": a["version"], "lockfile": a["lockfile"], "id": a["advisory"], "aliases": a.get("aliases") or [], "severity": a.get("severity") or "unrated", "summary": a.get("summary", ""), "fixed": a.get("fixed") or [], "cwes": a.get("cwes") or [], "kev": bool(a.get("kev")), "epss": a.get("epss"), "epss_percentile": a.get("epss_percentile")} for a in advisories]


def cmd_deps(args):
    positional, flags = parse_flags(args, {"root": "one"})
    root = os.path.abspath(flags["root"]) if isinstance(flags.get("root"), str) else os.getcwd()
    if flags.get("json"):
        return deps_json(root)
    if not os.path.isdir(root):
        fail("%s is not a folder" % root, "%s deps --root <project folder>" % tool())
    repo_files(root, [])
    rows, packages, not_parsed, why, problems = live_deps(root)
    if why:
        print(why[0].upper() + why[1:] + ".")
    else:
        for line in deps_lines(rows, packages, not_parsed):
            print(line)
        for problem in problems:
            print(problem + "; the priority goes without it.")
    if rows:
        print("Each vulnerable package: is the affected code called with outside data? (read graph://used-by/<function>, grep -rn \"<package>\" --include=*.*). Reachable: a confirmed finding with the upgrade as the fix; not reachable: a hardening note naming the upgrade.")
    print("Next: %s status" % tool())


def deps_json(root):
    """`deps --json`: the project's lockfiles and their known vulnerabilities, as one JSON object."""
    if not os.path.isdir(root):
        return json_out({"error": "%s is not a folder" % root}, 2)
    repo_files(root, [])
    lockfiles = sorted(set(getattr(repo_files, "lockfiles", [])))
    rows, packages, not_parsed, why, _ = live_deps(root, lockfiles, exploits=False)
    out = {"lockfiles": lockfiles, "not_parsed": not_parsed, "vulnerable": []}
    if why:
        out["skipped"] = why
    for (eco, name, version, lockfile), group in group_rows(rows):
        item = {"ecosystem": eco, "name": name, "version": version, "lockfile": lockfile, "ids": sorted({i for r in group for i in [r["id"]] + r["aliases"]}), "severity": group[0]["severity"]}
        fix = quick_fix_for(name, rows, lockfile)
        if fix:
            item["fixed"] = fix["to"]
            item["command"] = " && ".join(fix["commands"])
        out["vulnerable"].append(item)
    return json_out(out, 0)


def json_out(value, code):
    print(json.dumps(value, ensure_ascii=False))
    sys.exit(code)


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


def scan_lines(catalog, rel, lang, numbered, class_ids=None, markers=True, cache=None):
    """Pattern hits in `numbered` lines ([(line number, text)]) of one file: [(class id, rel, line,
    pattern name, raw line)]."""
    cache = {} if cache is None else cache
    key = (lang or "-", markers, tuple(sorted(class_ids)) if class_ids is not None else None)
    if key not in cache:
        pairs = [(cid, p) for cid, p in catalog.patterns_for(lang, markers) if class_ids is None or cid in class_ids]
        if lang is None:
            # Prose and unknown files: only secrets matter there.
            pairs = [(cid, p) for cid, p in pairs if cid == "obvious" and any(w in p["name"] for w in ("key", "token", "secret", "password", "connection", "JWT"))]
        cache[key] = pairs
    out = []
    for number, line in numbered:
        if len(line) > 600 or not line.strip():
            continue
        for cid, pattern in cache[key]:
            if pattern["needle"] and pattern["needle"] not in line:
                continue
            if pattern["re"].search(line):
                out.append((cid, rel, number, pattern["name"], line))
    return out


def scan_texts(catalog, texts, class_ids=None, markers=True):
    """Pattern hits per class: {class id: [(rel, line number, pattern name, raw line)]}."""
    hits = {}
    cache = {}
    for rel, (lang, text) in texts.items():
        for cid, rel_, number, name, line in scan_lines(catalog, rel, lang, enumerate(text.split("\n"), 1), class_ids, markers, cache):
            hits.setdefault(cid, []).append((rel_, number, name, line))
    for cls in catalog.classes:
        if class_ids is not None and cls["id"] not in class_ids:
            continue
        for spec in cls.get("files", []):
            if not spec.get("glob"):
                continue
            for rel in texts:
                if basename_matches(rel, spec["glob"]) and not rel.endswith((".example", ".sample", ".template")):
                    hits.setdefault(cls["id"], []).append((rel, 0, spec["name"], spec.get("note", "")))
    return hits


# ----------------------------------------------------------------------------- AI-agent files

AGENT_DIRS = {".claude", ".cursor", ".vscode", ".codex", ".continue", ".gemini", ".windsurf", ".amazonq", ".github"}
AGENT_CONFIGS = [".mcp.json", "mcp.json", ".cursor/mcp.json", ".vscode/mcp.json", "claude_desktop_config.json", ".claude/settings.json", ".claude/settings.*.json", ".claude/settings.local.json", ".codex/config.toml", ".continue/config.json", ".continue/config.yaml", ".gemini/settings.json", ".windsurf/mcp.json", ".amazonq/mcp.json", "opencode.json"]
AGENT_INSTRUCTIONS = ["AGENTS.md", "CLAUDE.md", "GEMINI.md", ".cursorrules", ".windsurfrules", ".github/copilot-instructions.md", "SKILL.md", ".cursor/rules/*.mdc", ".claude/commands/*.md", ".claude/agents/*.md", ".github/instructions/*.md", ".github/prompts/*.md"]
SERVER_KEYS = ("mcpServers", "servers", "mcp_servers", "context_servers", "mcp")
RUNNERS = {"npx": "npm", "bunx": "npm", "pnpx": "npm", "uvx": "pypi", "pipx": "pypi"}
BYPASS_FLAGS = ("--dangerously-skip-permissions", "--yolo", "--dangerously-bypass-approvals-and-sandbox", "--trust-all-tools", "--allow-all-tools")
DOWNLOAD_PLACE = re.compile(r"(^|[\s='\"])(/tmp/|/var/tmp/|/private/tmp/|~/Downloads/|\$HOME/Downloads/|/Users/[^/]+/Downloads/|/home/[^/]+/Downloads/|[A-Za-z]:\\Users\\[^\\]+\\Downloads\\)")
ZERO_WIDTH = {"\u200b", "\u200c", "\u200d", "\u2060", "\u2062", "\u2063", "\u2064", "\ufeff", "\u180e"}
BIDI = {chr(c) for c in list(range(0x202A, 0x202F)) + list(range(0x2066, 0x206A))}
WHITESPACE_RUN = re.compile(r"[ \t]{120,}\S")
HTML_COMMENT = re.compile(r"<!--(.*?)-->", re.S)


def glob_matches(rel, glob):
    if "/" not in glob:
        return fnmatch.fnmatch(os.path.basename(rel), glob)
    return fnmatch.fnmatch(rel, glob) or fnmatch.fnmatch(rel, "*/" + glob)


def agent_file_kind(rel):
    """`config` for an AI agent's settings or MCP server list, `instructions` for text an agent
    reads as its instructions (including skill files), None otherwise."""
    if any(glob_matches(rel, glob) for glob in AGENT_CONFIGS):
        return "config"
    if any(glob_matches(rel, glob) for glob in AGENT_INSTRUCTIONS):
        return "instructions"
    if ("/.claude/skills/" in "/" + rel or "/skills/" in "/" + rel) and rel.endswith((".md", ".mdc")):
        return "instructions"
    return None


def parse_toml(text):
    """TOML as data: the standard library's parser when there is one (3.11+), else the subset agent
    settings use (tables, dotted tables, `key = "string" | number | bool | [array]`, inline tables)."""
    try:
        import tomllib  # noqa: PLC0415 - optional, 3.11+
        return tomllib.loads(text)
    except ImportError:
        pass
    data, table = {}, None
    table = data
    for raw in text.split("\n"):
        line = raw.split(" #")[0].strip()
        if not line or line.startswith("#"):
            continue
        header = re.match(r"^\[\[?([^\]]+)\]\]?$", line)
        if header:
            table = data
            for part in [p.strip().strip('"') for p in header.group(1).split(".")]:
                table = table.setdefault(part, {})
            continue
        pair = re.match(r"^([\w\"'.-]+)\s*=\s*(.+)$", line)
        if pair:
            table[pair.group(1).strip("\"'")] = parse_scalar(pair.group(2).strip())
    return data


def parse_scalar(value):
    if value.startswith(("[", "{")):
        try:
            return json.loads(re.sub(r"(\w+)\s*=", r'"\1":', value.replace("'", '"')))
        except ValueError:
            return value
    if value[:1] in ("'", '"'):
        return value[1:-1] if len(value) > 1 and value[-1] == value[0] else value[1:]
    if value in ("true", "false"):
        return value == "true"
    try:
        return float(value) if "." in value else int(value)
    except ValueError:
        return value


def parse_yaml(text):
    """The YAML subset agent configs use: nested mappings, block lists of scalars or mappings, quoted
    and plain scalars, flow lists. Anchors, multi-line strings and several documents are not read."""
    tokens = []
    for raw in text.split("\n"):
        stripped = raw.strip()
        if not stripped or stripped.startswith("#") or stripped in ("---", "..."):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        while stripped == "-" or stripped.startswith("- "):
            tokens.append((indent, "-", None))
            stripped = stripped[2:].strip() if stripped != "-" else ""
            indent += 2
        if stripped:
            tokens.append((indent, "line", stripped))
    pos = 0

    def parse(indent):
        nonlocal pos
        if pos >= len(tokens):
            return None
        if tokens[pos][1] == "-":
            items = []
            while pos < len(tokens) and tokens[pos][0] == indent and tokens[pos][1] == "-":
                pos += 1
                items.append(parse(tokens[pos][0]) if pos < len(tokens) and tokens[pos][0] > indent else None)
            return items
        mapping = {}
        while pos < len(tokens) and tokens[pos][0] == indent and tokens[pos][1] == "line":
            line = tokens[pos][2]
            pos += 1
            pair = re.match(r"^([^:#]+?):(?:\s+(.*))?$", line)
            if not pair:
                if not mapping:
                    return parse_scalar(line)
                continue
            key, value = pair.group(1).strip().strip("\"'"), (pair.group(2) or "").split(" #")[0].strip()
            if value:
                mapping[key] = parse_scalar(value)
            elif pos < len(tokens) and (tokens[pos][0] > indent or (tokens[pos][0] == indent and tokens[pos][1] == "-")):
                mapping[key] = parse(tokens[pos][0])
            else:
                mapping[key] = None
        return mapping

    return (parse(tokens[0][0]) or {}) if tokens else {}


def line_of(text, needle, start_line=1):
    """The 1-based line where `needle` first appears at or after `start_line` (1 when it does not)."""
    if not needle:
        return start_line
    for number, line in enumerate(text.split("\n"), 1):
        if number >= start_line and needle in line:
            return number
    return 1


def servers_in(data):
    """Every MCP server an agent config declares: [(name, settings)]."""
    out = []

    def visit(node, depth):
        if depth > 6:
            return
        if isinstance(node, dict):
            for key, value in node.items():
                if key in SERVER_KEYS and isinstance(value, dict):
                    out.extend((name, conf) for name, conf in value.items() if isinstance(conf, dict))
                elif key in SERVER_KEYS and isinstance(value, list):
                    out.extend((conf.get("name") or "server %d" % i, conf) for i, conf in enumerate(value, 1) if isinstance(conf, dict))
                else:
                    visit(value, depth + 1)
        elif isinstance(node, list):
            for item in node:
                visit(item, depth + 1)
    visit(data, 0)
    return out


def strings_in(node, path=""):
    """Every (key path, string) in parsed data."""
    if isinstance(node, dict):
        for key, value in node.items():
            yield from strings_in(value, "%s.%s" % (path, key) if path else str(key))
    elif isinstance(node, list):
        for item in node:
            yield from strings_in(item, path + "[]")
    elif isinstance(node, str):
        yield path, node


def unpinned(command, args):
    """The package a runner starts without a pinned version (`npx -y pkg`, `uvx pkg`), or None."""
    base = os.path.basename(str(command or ""))
    words = [str(a) for a in args or []]
    if base in ("pnpm", "yarn") and words[:1] == ["dlx"]:
        base, words = "npx", words[1:]
    if base == "pipx" and words[:1] == ["run"]:
        words = words[1:]
    if base not in RUNNERS:
        return None
    pinned_from = next((words[i + 1] for i, w in enumerate(words[:-1]) if w == "--from"), None)
    package = pinned_from or next((w for w in words if not w.startswith("-")), None)
    if not package:
        return None
    if RUNNERS[base] == "npm":
        name, _, version = package[1:].partition("@") if package.startswith("@") else package.partition("@")
        ok = bool(version) and re.match(r"^\d", version) is not None
    else:
        ok = re.search(r"(==|@)\d", package) is not None
    return None if ok else package


def remote_host(url):
    try:
        parsed = urllib.parse.urlparse(url)
    except ValueError:
        return None
    if parsed.scheme not in ("http", "https", "ws", "wss") or not parsed.hostname:
        return None
    if parsed.hostname in ("localhost", "127.0.0.1", "::1", "0.0.0.0") or parsed.hostname.endswith(".localhost"):
        return None
    return "%s://%s" % (parsed.scheme, parsed.hostname)


def hidden_text_hits(rel, text):
    """Text an agent reads that a person does not see: zero-width and tag characters, bidirectional
    controls, HTML comments that say something, text pushed out of view by whitespace."""
    out = []
    for number, line in enumerate(text.split("\n"), 1):
        invisible = sum(1 for i, ch in enumerate(line) if (ch in ZERO_WIDTH and not (number == 1 and i == 0 and ch == "\ufeff")) or 0xE0000 <= ord(ch) <= 0xE007F)
        if invisible:
            out.append((rel, number, "agent instructions: hidden characters", "%d zero-width or tag character%s on this line (an agent reads them, a person does not see them)" % (invisible, "" if invisible == 1 else "s")))
        controls = sorted({"U+%04X" % ord(ch) for ch in line if ch in BIDI})
        if controls:
            out.append((rel, number, "agent instructions: bidirectional control characters", "%s reorders what a person sees on this line" % ", ".join(controls)))
        if WHITESPACE_RUN.search(line):
            out.append((rel, number, "agent instructions: text pushed out of view", "a run of %d spaces hides the text after it: %s" % (len(WHITESPACE_RUN.search(line).group(0)) - 1, shown(line.strip(), 100))))
    for match in HTML_COMMENT.finditer(text):
        words = match.group(1).split()
        if len(words) >= 6:
            number = text.count("\n", 0, match.start()) + 1
            out.append((rel, number, "agent instructions: HTML comment the reader does not see", shown(" ".join(words), 120)))
    return out


def agent_config_hits(rel, text):
    """The structural facts of an AI agent's settings or MCP server list that weaken it:
    [(rel, line, shape, text, signal or None)]."""
    name = os.path.basename(rel)
    try:
        if name.endswith(".toml"):
            data = parse_toml(text)
        elif name.endswith((".yaml", ".yml")):
            data = parse_yaml(text)
        else:
            data = json.loads(strip_jsonc(text))
    except (ValueError, TypeError) as error:
        return [(rel, 0, "agent config: not parsed", "the file is not valid %s (%s): read it by hand" % ("TOML" if name.endswith(".toml") else "YAML" if name.endswith((".yaml", ".yml")) else "JSON", str(error)[:80]), None)]
    out = []
    for server, conf in servers_in(data):
        at = line_of(text, '"%s"' % server) if '"%s"' % server in text else line_of(text, server)
        command, args = conf.get("command"), conf.get("args") or []
        if not isinstance(args, list):
            args = [args]
        if isinstance(command, list):
            command, args = (command[0] if command else ""), command[1:] + list(args)
        if isinstance(command, str) and " " in command.strip() and not args:
            command, *args = command.split()
        secrets, keys, strong = 0, [], False
        for section in ("env", "headers", "environment", "http_headers"):
            for key, value in (conf.get(section) or {}).items() if isinstance(conf.get(section), dict) else []:
                if not isinstance(value, str) or is_reference(value) or len(value.strip()) < 8:
                    continue
                key_format = any(regex.search(value) for regex in SECRET_VALUE_RES)
                if key_format or SECRET_NAME_RE.search(key) or BEARER_RE.search(value):
                    secrets += 1
                    keys.append(key)
                    strong = strong or key_format
        if secrets:
            out.append((rel, line_of(text, keys[0], at), "agent config: plaintext token", "server %s: %d secret value%s written in the file (%s); a reference to the environment keeps it out" % (server, secrets, "" if secrets == 1 else "s", ", ".join(keys[:4])), "key" if strong else None))
        package = unpinned(command, args)
        if package:
            out.append((rel, line_of(text, package, at), "agent config: unpinned package runner", "server %s: %s %s runs whatever version is published when it starts" % (server, os.path.basename(str(command)), package), None))
        for word in [str(command or "")] + [str(a) for a in args]:
            if DOWNLOAD_PLACE.search(word):
                out.append((rel, line_of(text, word.strip()[:40], at), "agent config: server started from a temporary or download folder", "server %s: %s" % (server, shown(word, 100)), None))
                break
        for key in ("url", "serverUrl", "httpUrl", "uri", "endpoint"):
            host = remote_host(conf.get(key)) if isinstance(conf.get(key), str) else None
            if host:
                plain = host.startswith(("http://", "ws://"))
                out.append((rel, line_of(text, conf[key][:40], at), "agent config: remote MCP server", "server %s: %s receives the agent's tool calls%s" % (server, host, " over plain http" if plain else ""), None))
                break
    bypass = []
    for path, value in strings_in(data):
        if any(flag in value for flag in BYPASS_FLAGS):
            bypass.append((value, "%s runs with %s" % (path, next(flag for flag in BYPASS_FLAGS if flag in value))))
        if path.endswith("defaultMode") and value == "bypassPermissions":
            bypass.append((value, "permissions.defaultMode is bypassPermissions"))
        if path.endswith(("approval_policy", "approvalPolicy")) and value == "never":
            bypass.append((value, "%s is never: every command runs without asking" % path))
        if path.endswith(("sandbox_mode", "sandboxMode")) and value == "danger-full-access":
            bypass.append((value, "%s is danger-full-access" % path))
        if re.search(r"(^|\.)(allow|autoApprove|alwaysAllow|allowedTools)\[\]$", path) and value.strip() in ("*", "Bash", "Bash(*)", "Bash(:*)", "mcp__*"):
            bypass.append((value, "%s allows %s" % (path[:-2], value.strip())))
    if isinstance(data, dict):
        for key in ("dangerouslySkipPermissions", "enableAllProjectMcpServers", "yolo", "autoAccept", "skipPermissions"):
            for path, value in flags_in(data, key):
                if value is True:
                    bypass.append((key, "%s is true" % path))
    for needle, what in bypass:
        out.append((rel, line_of(text, needle if len(needle) < 60 else needle[:60]), "agent config: permission checks turned off", what, None))
    return out


def flags_in(node, wanted, path=""):
    if isinstance(node, dict):
        for key, value in node.items():
            here = "%s.%s" % (path, key) if path else str(key)
            if key == wanted:
                yield here, value
            yield from flags_in(value, wanted, here)
    elif isinstance(node, list):
        for item in node:
            yield from flags_in(item, wanted, path + "[]")


def agent_hits(root, rels):
    """The agent-file checks of every listed file: [(rel, line, shape, text, signal or None)]."""
    out = []
    for rel in rels:
        kind = agent_file_kind(rel)
        text = read_text(os.path.join(root, rel))
        if not kind or text is None:
            continue
        if kind == "config":
            out.extend(agent_config_hits(rel, text))
        out.extend(hit + (None,) for hit in hidden_text_hits(rel, text))
    return out


# ----------------------------------------------------------------------------- the project's graph

# The audit lives in the project's graph (the memory plugin, through the app): its map, units, the
# lines to judge and how each was settled, the vulnerable dependencies, and the findings, which the
# Security tab lists by priority. Every write is checked: a refused one is a failure the model reads.

PRIORITY_BAND = {"critical": 400, "high": 300, "medium": 200, "low": 100, "informational": 50}
STATUS_DONE = ("fixed", "verified")
APP_AWAY = "the audit is kept in the project's graph, and CortexCode cannot be reached from here (%s)"
APP_NEXT = "start CortexCode, open this project, and run this from one of its sessions"


def graph_call(method, params):
    """One call to the app (`cx call`, the params on stdin): (True, result) or (False, why)."""
    home = os.environ.get("CORTEX_HOME")
    if not home:
        return False, "CORTEX_HOME is not set"
    cx = os.path.join(home, "bin", "cx")
    if not os.path.isfile(cx):
        return False, "cx is not installed in %s" % os.path.join(home, "bin")
    try:
        out = subprocess.run([cx, "call", method, "-"], input=json.dumps(params), capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired) as error:
        return False, "cx: %s" % error
    if out.returncode != 0:
        message = (out.stderr.strip() or out.stdout.strip() or "cx exited %d" % out.returncode).split("\n")[-1]
        return False, re.sub(r"^cx: (?:[a-z_]+: )?", "", message)
    try:
        return True, json.loads(out.stdout) if out.stdout.strip() else {}
    except ValueError:
        return False, "cx answered something that is not JSON"


def project_id(root):
    """(the app's id of the project at `root`, None) or (None, why): the `run` shell exports it;
    outside the app the project list is matched by root."""
    env = os.environ.get("CORTEX_PROJECT_ID")
    if env and os.path.realpath(root) == os.path.realpath(os.getcwd()):
        return env, None
    ok, result = graph_call("frame.project.list", {})
    if not ok:
        return None, result
    want = os.path.realpath(root)
    projects = (result or {}).get("projects", []) if isinstance(result, dict) else result if isinstance(result, list) else []
    for project in projects:
        if os.path.realpath(project.get("root", "")) == want:
            return project["id"], None
    return None, "no CortexCode project has the root %s" % want


def app_away(why):
    """Whether a failed call means the app itself is not there (rather than a refused write)."""
    return any(word in why for word in ("CORTEX_HOME", "cx is not installed", "cx:", "connect", "not running", "unavailable", "frame", "timed out", "No such file"))


def require_app(root):
    pid, why = project_id(root)
    if not pid:
        fail(APP_AWAY % why, APP_NEXT)
    return pid


def write(method, params, what):
    """A write to the graph; a refusal or failure ends the command with the store's reason."""
    ok, result = graph_call(method, params)
    if not ok:
        print("%s was not recorded: %s" % (what, result))
        if "Next:" not in result:
            print("Next: " + (APP_NEXT if app_away(result) else "%s status" % tool()))
        sys.exit(1)
    return result


class State:
    """The audit as the project's graph keeps it: the current audit, its units, its lines and how
    each was settled, its vulnerable dependencies, and the reports' folder."""

    def __init__(self, root, units=None):
        self.root = root
        self.pid = require_app(root)
        self.drafts = os.path.join(root, OUT_DEFAULT)
        self.drafts_rel = OUT_DEFAULT
        self.reload(units)

    def reload(self, units=None):
        params = {"project_id": self.pid}
        if units:
            params["units"] = units
        ok, result = graph_call("memory.security.audit.state", params)
        if not ok:
            fail("the audit could not be read from the project's graph: %s" % result, APP_NEXT if app_away(result) else "%s status" % tool())
        self.audit = (result or {}).get("audit")
        self.previous = (result or {}).get("previous")
        self.units = (result or {}).get("units") or []
        self.hits = {}
        for hit in (result or {}).get("hits") or []:
            self.hits.setdefault(hit["unit"].upper(), []).append(hit)
        self.by_hit = {hit["id"]: hit for hits in self.hits.values() for hit in hits}
        self.advisories = (result or {}).get("advisories") or []
        self._findings = None

    @property
    def audit_id(self):
        return (self.audit or {}).get("id")

    @property
    def out(self):
        return os.path.join(default_out(), self.audit_id or "audit")

    def path(self, name):
        return os.path.join(self.out, name)

    def coverage(self):
        if not self.audit:
            fail("no audit here yet", "%s next" % tool())
        coverage = {"root": self.audit.get("root") or self.root, "scope": [s for s in (self.audit.get("scope") or "").split(", ") if s], "files": self.audit.get("files", 0), "units": self.units, "audit_id": self.audit_id, "started_ms": self.audit.get("started_ms")}
        sync_units(self, coverage)
        return coverage

    def unit(self, coverage, uid):
        uid = uid.upper()
        for unit in coverage["units"]:
            if unit["id"] == uid:
                return unit
        fail("no unit %s (units: %s)" % (uid, ", ".join(u["id"] for u in coverage["units"])), "%s status" % tool())

    def rows(self):
        return rows_of(self.advisories)

    def findings(self):
        """The findings this audit reported last (every status), from the graph."""
        if self._findings is None:
            ok, result = graph_call("memory.security.findings", {"project_id": self.pid, "audit_id": self.audit_id})
            if not ok:
                fail("the audit's findings could not be read from the project's graph: %s" % result, "%s status" % tool())
            self._findings = (result or {}).get("findings") or []
        return self._findings

    def put_units(self, units):
        write("memory.security.units.put", {"project_id": self.pid, "audit_id": self.audit_id, "units": [unit_record(u) for u in units]}, "The unit")

    def put_hits(self, hits):
        for start in range(0, len(hits), HITS_WRITE_BATCH):
            write("memory.security.hits.put", {"project_id": self.pid, "audit_id": self.audit_id, "hits": hits[start:start + HITS_WRITE_BATCH]}, "The settlement")
        for hit in hits:
            known = self.by_hit.get(hit["id"])
            if known is not None:
                known.update(hit)

    def put_findings(self, findings, audit=True):
        """Store findings; returns (the findings as stored, the store's notes)."""
        result = write("memory.security.findings.put", {"project_id": self.pid, "audit_id": self.audit_id if audit else None, "findings": findings, "workspace": self.root}, "The finding")
        self._findings = None
        return (result or {}).get("findings") or [], (result or {}).get("notes") or []

    def put_audit(self, audit):
        write("memory.security.audit.put", {"project_id": self.pid, "audit": audit}, "The audit")
        self.audit = dict(self.audit or {}, **{k: v for k, v in audit.items() if v is not None})


def unit_record(unit):
    keys = ("id", "class", "name", "reason", "status", "note", "error", "hits", "files", "previous_status", "previous_note")
    return {key: unit.get(key) for key in keys if key in unit}


def fail(message, next_step=None):
    print(message)
    if next_step:
        print("Next: " + next_step)
    sys.exit(1)


def tool():
    """How the model invokes this tool (its own path, as given on the command line)."""
    return "python3 %s" % sys.argv[0]


def default_out():
    """Where the audit's reports live: under the app's home (nothing cleans it; the project's
    `tmp/` is wiped when a turn ends), one folder per project, one per audit inside it."""
    home = os.environ.get("CORTEX_HOME")
    root = os.getcwd()
    if not home:
        return os.path.join(root, OUT_DEFAULT)
    return os.path.join(home, "audits", "%s-%s" % (os.path.basename(root) or "project", hashlib.sha1(root.encode("utf-8")).hexdigest()[:8]))


LEGACY_FILES = ("coverage.json", "recon.json", "deps.json", "findings.json", "previous.json")
LEGACY_DIRS = ("hits", "hits-state")


def drop_legacy_state():
    """Remove the audit state earlier versions kept as files beside the reports (the graph holds
    it now): the JSON records and the hit folders, at the project's folder and in its `runs/`
    archives. The Markdown reports stay."""
    if not os.environ.get("CORTEX_HOME"):
        return
    base = default_out()
    folders = [base] + sorted(str(p) for p in Path(base, "runs").glob("*") if p.is_dir())
    for folder in folders:
        for name in LEGACY_FILES:
            try:
                os.remove(os.path.join(folder, name))
            except OSError:
                pass
        for name in LEGACY_DIRS:
            shutil.rmtree(os.path.join(folder, name), ignore_errors=True)


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
                    if flags_with_values[name] == "one":
                        break
                flags[name] = values if flags_with_values[name] == "many" else (values[0] if values else True)
            else:
                flags[name] = True
        else:
            positional.append(arg)
        i += 1
    return positional, flags


# ----------------------------------------------------------------------------- recon

def history_secrets(root, catalog):
    """Credential formats added in the last commits' diffs: [(short sha, file, pattern name, masked excerpt)]."""
    log = git(["log", "-p", "-n", str(HISTORY_COMMITS), "--no-color", "--format=commit %h", "--", "."], root, limit=30_000_000)
    if not log:
        return []
    patterns = [p for p in catalog.by_id["obvious"]["patterns"] if p["name"] in STRONG_PATTERNS or p["name"] in ("Google API key", "JWT literal")]
    rows, sha, path, seen = [], "", "", set()
    for line in log.split("\n"):
        if line.startswith("commit "):
            sha = line[7:].strip()
        elif line.startswith("+++ b/"):
            path = line[6:].strip()
        elif line.startswith("+") and not line.startswith("+++"):
            for pattern in patterns:
                if (not pattern["needle"] or pattern["needle"] in line) and pattern["re"].search(line):
                    if (path, pattern["name"]) not in seen:
                        seen.add((path, pattern["name"]))
                        rows.append((sha, path, pattern["name"], shown(line[1:], 100)))
                    break
        if len(rows) >= 30:
            break
    return rows


def hit_record(uid, index, cls_id, rel, number, shape, text, signal_kind=None, extra=None):
    """A line to judge as the graph keeps it; `signal_kind` names a strong host signal."""
    hit = {"id": "%s.%d" % (uid, index), "unit": uid, "class": cls_id, "path": rel or "", "line": int(number or 0), "shape": shape, "text": shown(text)}
    if signal_kind:
        hit["signal"] = {"key": "real key format (%s)" % shape, "history": "a secret in git history", "malicious": "malicious package", "kev": "exploited in the wild (CISA KEV)"}[signal_kind]
        hit["severity"] = SIGNAL_SEVERITY[signal_kind]
    hit.update(extra or {})
    return hit


def dependency_hits(uid, rows, root):
    """The dependencies unit's lines: one per vulnerable package, worst first."""
    out = []
    for index, ((eco, name, version, lockfile), group) in enumerate(group_rows(rows), 1):
        fix = covering_fix(group)
        extra = exploit_words(group)
        text = "%s %s %s: %d advisor%s, worst %s%s; %s; %s" % (eco, name, version, len(group), "y" if len(group) == 1 else "ies", group[0]["severity"], (", " + ", ".join(extra)) if extra else "", ("upgrade to %s" % fix) if fix else "no fixed version", "; ".join("%s %s" % (r["id"], r["summary"][:70]) for r in group[:2]))
        kind = "malicious" if any(r["id"].startswith("MAL-") for r in group) else "kev" if any(r.get("kev") for r in group) else None
        lock_text = read_text(os.path.join(root, lockfile)) or ""
        line = pin_line(lock_text, name, version) or 0
        hit = hit_record(uid, index, "dependencies", lockfile, line, "vulnerable package", text, kind, {"ecosystem": eco, "package": name, "version": version})
        if kind == "kev" and SEVERITY_RANK.get(group[0]["severity"], 5) < SEVERITY_RANK["high"]:
            hit["severity"] = group[0]["severity"]
        out.append(hit)
    return out


def pin_line(text, package, version):
    """The 1-based line of a lockfile that pins `package` at `version` (the memory plugin's rule)."""
    name = package.rsplit(":", 1)[-1]
    version = version.lstrip("v")
    if not name or not version:
        return None
    name_re = re.compile(r"(?<![A-Za-z0-9_.-])" + re.escape(name) + r"(?![A-Za-z0-9_./-])")
    version_re = re.compile(r"(?<![0-9.])" + re.escape(version) + r"(?![A-Za-z0-9.])")
    lines = text.split("\n")
    for index, line in enumerate(lines):
        if name_re.search(line) and any(version_re.search(l) for l in lines[index:index + 8]):
            return index + 1
    return None


def cmd_recon(args):
    positional, flags = parse_flags(args, {"scope": "many"})
    scope = list(flags.get("scope", [])) + positional
    root = os.getcwd()
    started = time.time()
    pid = require_app(root)
    catalog = Catalog()
    files = repo_files(root, scope)
    if not files:
        fail("no files to audit under %s%s" % (root, (" within " + ", ".join(scope)) if scope else ""), "run from the repository root, or name existing paths after --scope")
    git_head = (git(["rev-parse", "--short", "HEAD"], root) or "").strip()
    dirty = bool((git(["status", "--porcelain"], root) or "").strip())
    facts, texts = collect(root, files)
    hits = scan_texts(catalog, texts)
    agent_files = sorted(set(getattr(repo_files, "agent_files", [])))
    agent = agent_hits(root, agent_files)
    history = history_secrets(root, catalog)
    rows, packages, not_parsed, deps_why, exploit_problems = live_deps(root)

    # What the host sees of the findings already known, before the new audit's units are made.
    ok, recheck = graph_call("memory.security.recheck", {"project_id": pid, "workspace": root})
    if not ok:
        fail("the known findings could not be re-checked: %s" % recheck, APP_NEXT)
    checked = {row["id"]: row for row in (recheck or {}).get("checked") or []}
    ok, known = graph_call("memory.security.findings", {"project_id": pid, "status": ["open", "fixing", "accepted"]})
    if not ok:
        fail("the known findings could not be read: %s" % known, APP_NEXT)
    known = [f for f in (known or {}).get("findings") or [] if f.get("verdict") != "rejected"]
    ok, before = graph_call("memory.security.audit.state", {"project_id": pid})
    previous_units = {u["class"]: u for u in ((before or {}).get("units") or []) if u.get("status") not in ("planned", "in_progress")} if ok else {}

    units, unit_hits = [], {}
    if known:
        units.append({"id": "U0", "class": "previous", "name": PREVIOUS_CLASS["name"], "reason": "%d issues already known to the project" % len(known), "status": "planned", "note": ""})
        rows0 = []
        for index, finding in enumerate(known, 1):
            label = {"confirmed": "confirmed finding, %s" % (finding.get("severity_overall") or ""), "needs_validation": "lead (needs validation)", "hardening": "hardening note"}.get(finding.get("verdict"), finding.get("verdict"))
            now = checked.get(finding["id"], {})
            line = now.get("line") or finding.get("sink_line") or 0
            text = "%s: %s. Now: %s" % (label, (finding.get("title") or "")[:120], now.get("sentence") or "not compared")
            hit = hit_record("U0", index, "previous", finding.get("sink_path") or "", line, "previous " + {"confirmed": "finding", "needs_validation": "lead", "hardening": "hardening"}.get(finding.get("verdict"), "finding"), text, None, {"finding_id": finding["id"]})
            if finding.get("signal"):
                hit["signal"], hit["severity"] = finding["signal"], finding.get("host_severity")
            rows0.append(hit)
        unit_hits["U0"] = rows0
    for cls in catalog.classes:
        ok_, reason = applies_to(cls, facts, texts)
        if not ok_ and cls["id"] == "ai-llm-agent" and agent:
            ok_, reason = True, "AI-agent files: %s" % ", ".join(sorted({a[0] for a in agent})[:4])
        if not ok_:
            continue
        uid = "U%d" % (len([u for u in units if u["id"] != "U0"]) + 1)
        unit = {"id": uid, "class": cls["id"], "name": cls["name"], "reason": reason, "status": "planned", "note": ""}
        if cls["id"] in previous_units:
            unit["previous_status"] = previous_units[cls["id"]]["status"]
            unit["previous_note"] = previous_units[cls["id"]].get("note", "")
        if cls["id"] == "dependencies":
            lines = dependency_hits(uid, rows, root)
            if not lines:
                why = ("the dependency check did not run (%s): settle the unit blocked with that reason, or run recon again when the network is back" % deps_why) if deps_why else "no pinned dependency has a known vulnerability (OSV, live); settle the unit"
                lines = [hit_record(uid, 0, cls["id"], "", 0, "the unit", why)]
        else:
            raw = hits.get(cls["id"], [])
            lines = []
            for index, (rel, number, shape, text) in enumerate(raw, 1):
                lines.append(hit_record(uid, index, cls["id"], rel, number, shape, text, "key" if shape in STRONG_PATTERNS else None))
            if cls["id"] == "obvious":
                for sha, path, name, excerpt in history:
                    lines.append(hit_record(uid, len(lines) + 1, cls["id"], path, 0, "secret in git history", "commit %s: %s %s" % (sha, name, excerpt), "history" if name in STRONG_PATTERNS else None))
            if cls["id"] == "ai-llm-agent":
                for (rel, number, shape, text, sig) in agent:
                    lines.append(hit_record(uid, len(lines) + 1, cls["id"], rel, number, shape, text, sig))
            if not lines:
                lines = [hit_record(uid, 0, cls["id"], "", 0, "the unit", "no code shape of this class matched: read its 'look for' items against the entry points and handlers, then settle the unit")]
        unit["hits"] = sum(1 for h in lines if not h["id"].endswith(".0"))
        unit["files"] = len({h["path"] for h in lines if h["path"]})
        units.append(unit)
        unit_hits[uid] = lines
    if "U0" in unit_hits:
        units[0]["hits"] = len(unit_hits["U0"])
        units[0]["files"] = len({h["path"] for h in unit_hits["U0"] if h["path"]})

    stack = stack_of(facts)
    started_ms = int(time.time() * 1000)
    audit = {
        "id": "audit-%s" % hashlib.sha1(("%s %d" % (root, started_ms)).encode("utf-8")).hexdigest()[:12],
        "started_ms": started_ms, "units": len(units), "lines_judged": 0, "findings": 0, "leads": 0, "hardening": 0,
        "scope": ", ".join(scope) or None, "skill_version": SKILL_VERSION, "root": root, "git_head": git_head, "dirty": dirty, "files": len(files),
        "langs": ["%s=%d" % kv for kv in sorted(facts["langs"].items(), key=lambda kv: -kv[1])], "stack": stack, "entries": facts["entries"][:200],
        "manifests": ["%s=%s" % m for m in facts["manifests"]], "lockfiles": facts["lockfiles"], "not_parsed": not_parsed, "ci": facts["ci"], "containers": facts["containers"],
        "iac": facts["iac"], "secret_files": facts["secret_files"], "agent_files": agent_files, "tests": bool(facts["tests"]),
        "deps_checked_ms": None if deps_why else started_ms, "deps_packages": len(packages), "deps_note": deps_why,
    }
    state_after = write("memory.security.audit.start", {"project_id": pid, "audit": audit, "units": [unit_record(u) for u in units], "advisories": advisory_records(rows)}, "The audit")
    if not (state_after or {}).get("audit"):
        fail("the audit was not recorded: the store answered without it", "%s recon" % tool())
    all_hits = [hit for uid in [u["id"] for u in units] for hit in unit_hits.get(uid, [])]
    for start in range(0, len(all_hits), HITS_WRITE_BATCH):
        write("memory.security.hits.put", {"project_id": pid, "audit_id": audit["id"], "hits": all_hits[start:start + HITS_WRITE_BATCH]}, "The audit's lines")
    drop_legacy_state()

    # The map: what the project is, structurally.
    langs = sorted(facts["langs"].items(), key=lambda kv: -kv[1])
    code = [(LANG_NAMES.get(l, l), n) for l, n in langs if l in CODE_LANGS]
    other = [(LANG_NAMES.get(l, l), n) for l, n in langs if l not in CODE_LANGS]
    print("Project: %s%s, %d files%s%s" % (os.path.basename(root), (" at " + git_head) if git_head else "", len(files), " (scope: %s)" % ", ".join(scope) if scope else "", ", uncommitted changes present" if dirty else ""))
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
    if facts["manifests"]:
        parts = []
        for rel, kind in facts["manifests"][:8]:
            locks = MANIFEST_LOCK.get(os.path.basename(rel), [])
            folder = os.path.dirname(rel)
            locked = any(os.path.dirname(lf) == folder and os.path.basename(lf) in locks for lf in facts["lockfiles"]) if locks else None
            parts.append(rel + ("" if locked is None else (" (locked)" if locked else " (NO LOCKFILE)")))
        print("Dependencies: " + ", ".join(parts))
    if facts["ci"] or facts["containers"] or facts["iac"]:
        print("Delivery: " + ", ".join(facts["ci"][:4] + facts["containers"][:4] + facts["iac"][:4]) + (" ..." if len(facts["ci"]) + len(facts["containers"]) + len(facts["iac"]) > 12 else ""))
    if facts["secret_files"]:
        print("Secret-looking files in the repository: " + ", ".join(facts["secret_files"][:8]))
    if agent_files:
        print("AI-agent files: " + ", ".join(agent_files[:8]) + (" ..." if len(agent_files) > 8 else ""))
    if not facts["tests"]:
        print("Tests: none found (local checks will be small scripts in tmp/)")
    if deps_why:
        print(deps_why[0].upper() + deps_why[1:] + ".")
    else:
        for line in deps_lines(rows, packages, not_parsed, RECON_DEPS_SHOWN):
            print(line)
        for problem in exploit_problems:
            print(problem + "; the priority goes without it.")
    moved = [row for row in checked.values() if row.get("now")]
    if moved:
        print("Re-checked by the host: " + "; ".join("%s now %s (%s)" % (row["id"], row["now"], row["sentence"]) for row in moved[:6]) + ("; and %d more" % (len(moved) - 6) if len(moved) > 6 else "") + ".")
    if known:
        print("")
        print("%d issues already known to this project are U0, re-checked first." % len(known))
    print("")
    print("Units (the audit's checklist; each is one attack class that applies here):")
    for unit in units:
        line = "  %s %s: %s; %d suspicious lines in %d files" % (unit["id"], unit["class"], unit["reason"], unit["hits"], unit["files"])
        if unit.get("previous_status"):
            line += "; last audit: %s" % unit["previous_status"]
        print(line)
    print("")
    print("Recorded in the project's graph as %s. %.1fs." % (audit["id"], time.time() - started))
    print("Next: %s next" % tool())


# ----------------------------------------------------------------------------- hunt, cover, status

def print_hits(state, unit, show_all):
    rows = hit_rows(state, unit)
    real = [r for r in rows if not r[0].endswith(".0")]
    if not real:
        print(rows[0][4] if rows else "No line to judge.")
        return
    by_pattern = {}
    for hid, rel, number, shape, text in real:
        by_pattern.setdefault(shape, []).append((hid, rel, number, text))
    print("Suspicious lines (%d in %d files), by shape:" % (len(real), len({r[1] for r in real})))
    shown_count = 0
    for name, items in sorted(by_pattern.items(), key=lambda kv: -len(kv[1])):
        print("  %s (%d):" % (name, len(items)))
        for hid, rel, number, text in (items if show_all else items[:HITS_PER_PATTERN]):
            if not show_all and shown_count >= HITS_SHOWN:
                break
            print("    %s %s%s  %s" % (hid, rel, ":%d" % number if number else "", text[:110]))
            shown_count += 1
        if not show_all and len(items) > HITS_PER_PATTERN:
            print("    ... %d more" % (len(items) - HITS_PER_PATTERN))
    if not show_all and shown_count < len(real):
        print("All %d lines: %s hunt %s --all" % (len(real), tool(), unit["id"]))


def cmd_hunt(args):
    positional, flags = parse_flags(args, {})
    if not positional:
        fail("which unit?", "%s hunt U1" % tool())
    state = State(os.getcwd())
    coverage = state.coverage()
    unit = state.unit(coverage, positional[0])
    cls = Catalog().by_id[unit["class"]]
    if unit["status"] == "planned":
        unit["status"] = "in_progress"
        state.put_units([unit])
    print("%s %s (%s)" % (unit["id"], cls["name"], unit["reason"]))
    print("Invariant: " + cls["invariant"])
    print("Look for:")
    for item in cls["look"]:
        print("  - " + item)
    print("A finding here: " + cls["counts"])
    print("Only a hardening note: " + cls["hardening"])
    if unit["class"] == "obvious":
        for rel in (state.audit or {}).get("secret_files") or []:
            print("Secret-looking file: %s" % rel)
    print_hits(state, unit, bool(flags.get("all")))
    if unit["class"] == "dependencies":
        print("Each vulnerable package: is the affected code called with outside data? (read graph://used-by/<function>, grep -rn \"<package>\"). Reachable: a confirmed finding with the upgrade as the fix; not reachable: a hardening note naming the upgrade.")
    print("")
    print("For each line: find where its value comes from (read graph://used-by/<function>, read <path>:<from>-<to>), decide whether a lower-trust party controls it and whether a check you read stops it; then settle it (%s settle <id> ...)." % tool())
    print("Next, when every line is settled: %s cover %s covered|candidate|blocked \"<what settled it>\"" % (tool(), unit["id"]))


def cmd_cover(args):
    if len(args) < 2 or args[1] not in ("covered", "candidate", "blocked"):
        fail("usage: cover <unit> covered|candidate|blocked \"<note>\"", "%s status" % tool())
    state = State(os.getcwd())
    coverage = state.coverage()
    unit = state.unit(coverage, args[0])
    status, note = args[1], " ".join(args[2:]).strip()
    if status == "blocked" and not note:
        fail("a blocked unit needs the note saying what could not be read or run", "%s cover %s blocked \"<why>\"" % (tool(), unit["id"]))
    if status == "candidate":
        recorded = [f for f in state.findings() if f.get("class") == unit["class"] or f.get("unit") == unit["id"]]
        if not recorded:
            fail("candidate means records exist for this class; none names %s yet" % unit["class"], "%s new confirmed|needs_validation|rejected, then finding <file>, then cover again" % tool())
    strong = [h for h in state.hits.get(unit["id"], []) if h.get("signal") and h.get("verdict") not in ("finding", "lead")]
    if strong and status != "candidate":
        fail("%s holds strong host signals not settled as finding or lead: %s" % (unit["id"], ", ".join("%s (%s)" % (h["id"], h["signal"]) for h in strong[:4])), "%s settle %s finding" % (tool(), strong[0]["id"]))
    unit["status"], unit["note"] = status, note
    state.put_units([unit])
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress") and u["id"] != unit["id"]]
    print("%s %s: %s%s" % (unit["id"], unit["class"], status, (" (" + note + ")") if note else ""))
    if left:
        print("Left: " + ", ".join("%s %s" % (u["id"], u["class"]) for u in left))
        print("Next: %s hunt %s" % (tool(), left[0]["id"]))
    else:
        print("Every unit is settled.")
        print("Next: %s report" % tool())


def session_findings(pid):
    """The findings this session fixes or fixed: (open ones, all of them), or exits 1 when the graph
    cannot be read (a guard that cannot read the record never passes)."""
    session = os.environ.get("CORTEX_SESSION_ID")
    params = {"project_id": pid, "session_id": session} if session else {"project_id": pid, "status": ["fixing"]}
    ok, result = graph_call("memory.security.findings", params)
    if not ok:
        fail("the fix could not be read from the project's graph: %s" % result, APP_NEXT)
    found = (result or {}).get("findings") or []
    return [f for f in found if f.get("status") in ("open", "fixing")], found


def cmd_status(args):
    positional, flags = parse_flags(args, {"finding": "one", "unit": "one"})
    root = os.getcwd()
    # The fix session's guard: exit 0 once its finding is recorded fixed.
    if flags.get("fixing") or flags.get("finding"):
        pid = require_app(root)
        if isinstance(flags.get("finding"), str):
            ok, result = graph_call("memory.security.finding", {"project_id": pid, "id": flags["finding"]})
            if not ok:
                fail("finding %s could not be read from the project's graph: %s" % (flags["finding"], result), "%s show %s" % (tool(), flags["finding"]))
            finding = (result or {}).get("finding") or {}
            if finding.get("status") in STATUS_DONE + ("accepted", "gone"):
                print("%s is %s." % (finding["id"], finding["status"]))
                return
            print("Open: %s is %s: %s" % (finding.get("id", flags["finding"]), finding.get("status", "open"), finding.get("title", "")))
            print("Next: make the fix, run the test, then %s resolve %s \"<how>\"" % (tool(), finding.get("id", flags["finding"])))
            sys.exit(1)
        open_fixes, _ = session_findings(pid)
        if not open_fixes:
            print("No fix is open in this session.")
            return
        for finding in open_fixes:
            print("Open: %s (%s) %s; sink %s:%s" % (finding["id"], finding.get("severity_overall") or finding.get("verdict"), finding.get("title", ""), finding.get("sink_path") or "", finding.get("sink_line") or ""))
        print("Next: make the fix at the sink, run the test, then %s resolve %s \"<how you verified it>\"" % (tool(), open_fixes[0]["id"]))
        sys.exit(1)
    state = State(root)
    coverage = state.coverage()
    states = hit_states(state)
    if isinstance(flags.get("unit"), str):
        unit = state.unit(coverage, flags["unit"])
        rows = hit_rows(state, unit)
        left = [r[0] for r in rows if r[0] not in states]
        if left:
            print("Open: %s %s, %d of %d lines (%s%s)." % (unit["id"], unit["class"], len(left), len(rows), ", ".join(left[:6]), ", ..." if len(left) > 6 else ""))
            print("Next: %s next --unit %s" % (tool(), unit["id"]))
            sys.exit(1)
        report_path = state.path(os.path.join("units", unit_report(state, coverage, unit, Catalog())))
        print("%s is settled: %s" % (unit["id"], unit_line(state, unit, Catalog(), report_path)))
        return
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
    findings = state.findings()
    counts = {v: sum(1 for f in findings if f.get("verdict") == v) for v in VERDICTS}
    if findings or not open_units:
        print("Records: %d confirmed, %d needs_validation, %d rejected" % (counts["confirmed"], counts["needs_validation"], counts["rejected"]))
    pending = pending_records(state)
    if pending:
        print("Drafts not recorded yet: " + ", ".join(os.path.join(state.drafts_rel, name) for name, _ in pending))
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress")]
    if pending or left or not report_current(state):
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
    root = os.getcwd()
    drafts = os.path.join(root, OUT_DEFAULT)
    os.makedirs(drafts, exist_ok=True)
    n = 1
    while os.path.exists(os.path.join(drafts, "finding-%d.json" % n)):
        n += 1
    name = "finding-%d.json" % n
    with open(os.path.join(drafts, name), "w", encoding="utf-8") as handle:
        json.dump(TEMPLATES[args[0]], handle, indent=1)
    rel = os.path.join(OUT_DEFAULT, name)
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
    state = State(root)
    if not state.audit:
        fail("no audit is open to record this in", "%s next" % tool())
    hit = state.by_hit.get(str(record.get("hit") or "").upper()) if record.get("hit") else None
    if record.get("hit") and (hit is None or record.get("audit") not in (None, state.audit_id)):
        # A draft from an earlier audit: the record stands on its own, without the old line.
        record.pop("hit", None)
        record.pop("unit", None)
        hit = None
    for step in record.get("trace", []):
        lines = (read_text(os.path.join(root, step["file"])) or "").split("\n")
        if 0 < step.get("line", 0) <= len(lines):
            step["text"] = lines[step["line"] - 1]
    finding = record_to_finding(record, state.rows(), hit)
    if hit and hit.get("severity") and record["verdict"] == "confirmed":
        floor = hit["severity"]
        if SEVERITY.index(record["severity"]["overall"]) < SEVERITY.index(floor):
            print("Severity set to %s: the host sets it for %s; it can be raised, not lowered." % (floor, hit.get("signal")))
    stored, notes = state.put_findings([finding])
    for note in notes:
        print(note[0].upper() + note[1:] + ".")
    if hit:
        verdict = {"confirmed": "finding", "needs_validation": "lead"}.get(record["verdict"], "safe")
        settled = dict(hit, verdict=verdict, note=hit.get("note") or record["title"], finding_id=record["fingerprint"], record=os.path.basename(path), settled_ms=int(time.time() * 1000))
        if verdict == "safe":
            settled["note"] = "rejected: " + record.get("reason", record["title"])
            settled["record"] = os.path.basename(path)
        state.put_hits([settled])
    print("Recorded %s as %s in the project's graph." % (record["fingerprint"], record["verdict"]))
    print("Next: %s next" % tool())


def record_to_finding(record, deps_rows, hit=None):
    """A validated record (confirmed, needs_validation or rejected) as the graph's finding."""
    trace = [{"kind": step.get("kind", "step"), "file": step.get("file", ""), "line": int(step.get("line") or 0), "what": step.get("what", "")} for step in record.get("trace", [])]
    sink = record.get("trace", [{}])[-1] if record.get("trace") else {}
    verdict = record["verdict"]
    severity = dict(record.get("severity") or {})
    fix = record.get("fix") or {}
    evidence = record.get("evidence") or {}
    floor = (hit or {}).get("severity")
    if floor and verdict == "confirmed" and severity.get("overall") in SEVERITY and SEVERITY.index(severity["overall"]) < SEVERITY.index(floor):
        severity["overall"] = floor
        if severity.get("impact") in SEVERITY and SEVERITY.index(severity["impact"]) < SEVERITY.index(floor):
            severity["impact"] = floor
    quick_fix, package, version = None, None, None
    if record.get("class") == "dependencies":
        parts = record["fingerprint"].split(":")
        package = (hit or {}).get("package") or (parts[-1] if len(parts) >= 3 else None)
        version = (hit or {}).get("version") or next((r["version"] for r in deps_rows if r["name"].lower() == (package or "").lower()), None)
        if package:
            quick_fix = quick_fix_for(package, deps_rows)
    group = [r for r in deps_rows if package and r["name"].lower() == package.lower()]
    finding = {
        "id": record["fingerprint"],
        "class": record.get("class", ""),
        "verdict": verdict,
        "title": record.get("title", ""),
        "principal": record.get("principal", ""),
        "control": record.get("control", ""),
        "impact": record.get("impact", ""),
        "result": mask((evidence.get("observed") or record.get("impact") or "").split("\n")[0]),
        "evidence_how": mask(evidence.get("how", "")),
        "evidence_observed": mask(evidence.get("observed", "")),
        "severity_likelihood": severity.get("likelihood"),
        "severity_impact": severity.get("impact"),
        "severity_overall": severity.get("overall") if verdict == "confirmed" else (floor if floor and verdict == "needs_validation" else None),
        "fix_where": fix.get("where", ""),
        "fix_change": fix.get("change", ""),
        "fix_test": fix.get("test", ""),
        "missing_fact": ("%s. Check: %s" % (record.get("blocker", "").rstrip("."), record.get("check", ""))) if verdict == "needs_validation" else None,
        "blocker": record.get("blocker"),
        "check_how": record.get("check"),
        "reason": record.get("reason"),
        "conditions": [str(c) for c in record.get("conditions") or []],
        "trace": trace,
        "quick_fix": quick_fix,
        "source": "audit",
        "unit": record.get("unit"),
        "hit": record.get("hit"),
        "signal": (hit or {}).get("signal"),
        "host_severity": floor,
        "advisories": sorted({r["id"] for r in group}),
        "kev": any(r.get("kev") for r in group),
        "epss": max((r.get("epss") or 0 for r in group), default=0) or None,
        "sink_path": sink.get("file") or None,
        "sink_line": int(sink.get("line") or 0) or None,
        "sink_text": shown(sink.get("text") or "") or None,
        "sink_hash": line_hash(sink.get("text") or "") if sink.get("text") else None,
        "sink_package": package if record.get("class") == "dependencies" else None,
        "sink_version": version if record.get("class") == "dependencies" else None,
    }
    finding["priority"], finding["priority_reason"] = priority_of(finding)
    return finding


def priority_of(finding):
    """One number, higher first, from the facts the finding states; the reason spells the terms out.
    The store adds the age term (+1 per day open, at most 30) when it lists."""
    terms = []
    verdict = finding.get("verdict")
    level = (finding.get("severity_overall") or "").lower()
    if verdict == "confirmed":
        level = level if level in PRIORITY_BAND else "low"
        score = PRIORITY_BAND[level]
        terms.append("%s severity %d" % (level, score))
        score += 50
        terms.append("confirmed +50")
    elif verdict == "needs_validation":
        if level in PRIORITY_BAND:
            score = PRIORITY_BAND[level]
            terms.append("lead, %s severity by the host %d" % (level, score))
        else:
            score = 150
            terms.append("lead 150")
    elif verdict == "rejected":
        return 0, "rejected claim"
    else:
        level = level if level in PRIORITY_BAND else "low"
        score = PRIORITY_BAND[level] - 80
        terms.append("hardening note, %s band %d -80" % (level, PRIORITY_BAND[level]))
    who = (finding.get("principal") or "").lower()
    if any(word in who for word in ("anonymous", "unauthenticated", "any visitor", "remote peer", "the network")):
        score += 30
        terms.append("anonymous party +30")
    elif any(word in who for word in ("user", "authenticated", "logged", "member", "tenant")):
        score += 15
        terms.append("authenticated party +15")
    if finding.get("quick_fix"):
        score += 20
        terms.append("quick fix available +20")
    if finding.get("kev"):
        score += 80
        terms.append("exploited in the wild (CISA KEV) +80")
    epss = finding.get("epss") or 0
    bonus = int(round(epss * 40))
    if bonus:
        score += bonus
        terms.append("EPSS %d%% +%d" % (round(epss * 100), bonus))
    return score, ", ".join(terms)


ECOSYSTEM_TOOLS = {
    "npm": lambda name, to, lockdir: ["cd %s && npm install %s@%s" % (lockdir, name, to)],
    "crates.io": lambda name, to, lockdir: ["cd %s && cargo update -p %s --precise %s" % (lockdir, name, to)],
    "PyPI": lambda name, to, lockdir: ["cd %s && python3 -m pip install '%s==%s' && python3 -m pip freeze | grep -i '^%s==' " % (lockdir, name, to, name)],
    "Go": lambda name, to, lockdir: ["cd %s && go get %s@v%s && go mod tidy" % (lockdir, name, to.lstrip("v"))],
    "RubyGems": lambda name, to, lockdir: ["cd %s && bundle update %s" % (lockdir, name)],
    "Packagist": lambda name, to, lockdir: ["cd %s && composer update %s" % (lockdir, name)],
    "Maven": lambda name, to, lockdir: ["edit the version of %s to %s in %s/pom.xml, then: cd %s && mvn -q dependency:resolve" % (name, to, lockdir, lockdir)],
    "NuGet": lambda name, to, lockdir: ["cd %s && dotnet add package %s --version %s" % (lockdir, name, to)],
}


def quick_fix_for(package_name, deps_rows, lockfile=None):
    """The exact upgrade for a vulnerable package: the covering version and the commands of its
    ecosystem; None when no fixed version exists."""
    rows = [r for r in deps_rows if r["name"].lower() == package_name.lower() and (lockfile is None or r["lockfile"] == lockfile)]
    if not rows:
        return None
    to = covering_fix(rows)
    if not to:
        return None
    row = rows[0]
    lockdir = os.path.dirname(row["lockfile"]) or "."
    make = ECOSYSTEM_TOOLS.get(row["eco"])
    commands = make(row["name"], to, lockdir) if make else ["upgrade %s to %s in %s" % (row["name"], to, row["lockfile"])]
    if os.path.basename(row["lockfile"]).startswith("requirements") and row["lockfile"].endswith(".txt"):
        # Portable on macOS and Linux (no `sed -i`): Python rewrites the pin in place.
        pattern = r"(?im)^(%s\s*(?:\[[^]]*\])?\s*===?\s*)%s(?![\w.])" % (re.escape(row["name"]), re.escape(row["version"]))
        script = 'import re,sys; p=sys.argv[1]; t=open(p).read(); open(p, "w").write(re.sub(r"%s", lambda m: m.group(1) + "%s", t))' % (pattern, to)
        commands = ["python3 -c '%s' %s" % (script, row["lockfile"]), "cd %s && python3 -m pip install -r %s" % (lockdir, os.path.basename(row["lockfile"]))]
    return {"kind": "upgrade", "ecosystem": row["eco"], "package": row["name"], "from": row["version"], "to": to, "lockfile": row["lockfile"], "commands": commands}


def hardening_findings(state, coverage):
    """Every hardening note as a finding of its own: the class, the line, the reviewer's note."""
    deps_rows = state.rows()
    states = hit_states(state)
    out = []
    for unit in coverage.get("units", []):
        if unit["class"] == "previous":
            continue
        for hid, rel, number, shape, text in hit_rows(state, unit):
            judged = states.get(hid)
            if not judged or judged.get("status") != "hardening":
                continue
            hit = state.by_hit.get(hid) or {}
            package = hit.get("package")
            quick_fix = quick_fix_for(package, deps_rows, rel or None) if package else None
            group = [r for r in deps_rows if package and r["name"] == package and (not rel or r["lockfile"] == rel)]
            ident = ("hardening:%s:%s" % (unit["class"], package or ("%s:%d" % (rel or unit["id"], number or 0)))).lower()
            raw = (read_text(os.path.join(state.root, rel)) or "").split("\n") if rel and number else []
            sink_raw = raw[number - 1] if raw and 0 < number <= len(raw) else ""
            finding = {
                "id": ident,
                "class": unit["class"],
                "verdict": "hardening",
                "title": judged.get("note", "") or "hardening note",
                "principal": "",
                "control": "",
                "impact": "",
                "result": "",
                "evidence_how": "static: the reviewer's note on the flagged line",
                "evidence_observed": text[:200],
                "severity_overall": group[0]["severity"] if group and group[0]["severity"] in PRIORITY_BAND else None,
                "fix_where": ("%s:%d" % (rel, number)) if rel and number else (rel or ""),
                "fix_change": judged.get("note", ""),
                "fix_test": "",
                "trace": [{"kind": "sink", "file": rel or "", "line": int(number or 0), "what": text[:160]}] if rel else [],
                "quick_fix": quick_fix,
                "source": "audit",
                "unit": unit["id"],
                "hit": hid,
                "advisories": sorted({r["id"] for r in group}),
                "kev": any(r.get("kev") for r in group),
                "epss": max((r.get("epss") or 0 for r in group), default=0) or None,
                "sink_path": rel or None,
                "sink_line": int(number or 0) or None,
                "sink_text": shown(sink_raw) if sink_raw else None,
                "sink_hash": line_hash(sink_raw) if sink_raw else None,
                "sink_package": package,
                "sink_version": hit.get("version") if package else None,
            }
            finding["priority"], finding["priority_reason"] = priority_of(finding)
            out.append(finding)
    return out


# ----------------------------------------------------------------------------- report

def md_escape(text):
    return str(text).replace("|", "\\|").replace("\n", " ")


WATCH_TITLE = "Security: dependency watch"
WATCH_WHEN = "every day at 09:00"


def ensure_watch(state):
    """After a whole audit, the project's locked dependencies are watched daily for new advisories
    (a command routine of the scheduler running `watch`; new high, critical or exploited ones reach
    the person as notifications). One routine per project; an offline project gets none. Returns the
    line to print when it was made now, else None (also when the scheduler is not there: the audit
    stands without it)."""
    if offline_here(state.root):
        return None
    ok, listed = graph_call("schedule.list", {"scope": state.pid})
    if not ok:
        return None
    routines = (listed or {}).get("routines", []) if isinstance(listed, dict) else []
    if any(r.get("title") == WATCH_TITLE and r.get("state") != "ended" for r in routines):
        return None
    # Absolute paths: the routine may run in the CortexTerm on this Mac, whose shell has no
    # CORTEX_HOME of its own.
    home = os.environ.get("CORTEX_HOME", "")
    command = "%spython3 %s watch --root %s" % ("CORTEX_HOME=%s " % shlex.quote(home) if home else "", shlex.quote(str(Path(__file__).resolve())), shlex.quote(state.root))
    params = {"scope": state.pid, "title": WATCH_TITLE, "kind": "command", "when": WATCH_WHEN, "command": command}
    ok, _ = graph_call("schedule.add", params)
    if not ok:
        return None
    return "The project's dependencies are now checked for new advisories %s (routine \"%s\"; new high, critical or exploited ones notify)." % (WATCH_WHEN, WATCH_TITLE)


def cmd_report(args):
    positional, flags = parse_flags(args, {})
    state = State(os.getcwd())
    coverage = state.coverage()
    audit = state.audit
    left = [u for u in coverage["units"] if u["status"] in ("planned", "in_progress")]
    if left and not flags.get("partial"):
        print("Not every unit is settled: " + ", ".join("%s %s (%s)" % (u["id"], u["class"], u["status"]) for u in left))
        fail("hunt them, or mark them blocked with why; `report --partial` writes a report that says they were not covered", "%s hunt %s" % (tool(), left[0]["id"]))
    # The hardening notes become findings first, so the report and the finish see them.
    hardening = hardening_findings(state, coverage)
    if hardening:
        state.put_findings(hardening)
    findings = state.findings()
    confirmed = sorted([f for f in findings if f["verdict"] == "confirmed"], key=lambda f: -SEVERITY.index(f.get("severity_overall") or "low"))
    leads = [f for f in findings if f["verdict"] == "needs_validation"]
    rejected = [f for f in findings if f["verdict"] == "rejected"]
    lines = []
    add = lines.append
    add("# Security audit: %s" % os.path.basename(coverage["root"]))
    add("")
    add("Source: %s%s. Scope: %s. %d files read. Method: source review of every attack class that applies to this stack, each suspicious line traced to its source and its effect; local checks with dummy data only; nothing deployed was touched." % (audit.get("git_head") or "working tree", " (uncommitted changes present)" if audit.get("dirty") else "", ", ".join(coverage.get("scope") or ["whole repository"]), coverage.get("files", 0)))
    if audit.get("stack"):
        add("Stack: " + ", ".join(audit["stack"]) + ".")
    if left:
        add("")
        add("**Partial pass.** Not covered: " + ", ".join("%s (%s)" % (u["class"], u["status"]) for u in left) + ".")
    blocked = [u for u in coverage["units"] if u["status"] == "blocked"]
    add("")
    add("## Summary")
    add("")
    counts = {level: sum(1 for f in confirmed if f.get("severity_overall") == level) for level in SEVERITY}
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
            add("| %s | %s | %s:%d | %s |" % (f.get("severity_overall"), md_escape(f["title"]), f.get("sink_path") or "", f.get("sink_line") or 0, md_escape(f.get("impact", ""))))
        for f in confirmed:
            add("")
            add("### %s: %s" % (f.get("severity_overall"), f["title"]))
            add("")
            add("- Fingerprint: `%s`" % f["id"])
            add("- Attacker: " + f.get("principal", ""))
            add("- Path:")
            for step in f.get("trace") or []:
                add("  - %s `%s:%d`: %s" % (step["kind"], step["file"], step["line"], step["what"]))
            add("- Control: " + f.get("control", ""))
            if f.get("conditions"):
                add("- Conditions: " + "; ".join(f["conditions"]))
            add("- Evidence: %s. Observed: %s" % (f.get("evidence_how", ""), f.get("evidence_observed", "")))
            add("- Impact: " + f.get("impact", ""))
            add("- Severity: likelihood %s, impact %s, overall %s" % (f.get("severity_likelihood"), f.get("severity_impact"), f.get("severity_overall")))
            add("- Fix: in %s, %s. Test: %s" % (f.get("fix_where", ""), f.get("fix_change", ""), f.get("fix_test", "")))
    previous_unit = next((u for u in coverage["units"] if u["class"] == "previous"), None)
    if previous_unit:
        rows0 = hit_rows(state, previous_unit)
        fixed = sum(1 for r in rows0 if (judged_all.get(r[0]) or {}).get("status") == "fixed")
        still = sum(1 for r in rows0 if (judged_all.get(r[0]) or {}).get("status") in ("finding", "lead", "hardening", "still"))
        add("")
        add("## Issues already known")
        add("")
        add("%d issues were known when this audit began; %d are fixed, %d are still open, %d were not re-checked." % (len(rows0), fixed, still, len(rows0) - fixed - still))
        add("")
        add("| Line | Issue | Where | Verdict |")
        add("|---|---|---|---|")
        for hid, rel, number, shape, text in rows0:
            st = judged_all.get(hid) or {}
            where = ("%s:%d" % (rel, number)) if rel and number else (rel or "")
            add("| %s | %s | %s | %s |" % (hid, md_escape(text[:140]), where, md_escape(VERDICT_WORDS.get(st.get("status"), st.get("status") or "not re-checked") + ((": " + st["note"]) if st.get("note") else ""))))
    deps = state.rows()
    if deps:
        add("")
        add("## Known vulnerable dependencies (OSV)")
        add("")
        add("| Package | Version | Advisory | Severity | Exploitation | Fixed in | Lockfile |")
        add("|---|---|---|---|---|---|---|")
        for row in deps:
            add("| %s %s | %s | %s%s %s | %s | %s | %s | %s |" % (row["eco"], row["name"], row["version"], row["id"], (" / " + row["aliases"][0]) if row["aliases"] else "", md_escape(row["summary"]), row["severity"], ", ".join(exploit_words([row])) or "-", ", ".join(sorted(set(row["fixed"]))[:3]) or "none listed", row["lockfile"]))
        add("")
        add("Reachability of each advisory is in the findings above when it was traced; an upgrade to the fixed version is the hardening note otherwise.")
    elif audit.get("deps_note"):
        add("")
        add("## Dependencies")
        add("")
        add(sentence(audit["deps_note"]))
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
            add("| %s | %s:%d | %s | %s |" % (md_escape(f["title"]), f.get("sink_path") or "", f.get("sink_line") or 0, md_escape(f.get("blocker") or f.get("missing_fact") or ""), md_escape(f.get("check_how") or "")))
    notes = [(u["class"], u["note"]) for u in coverage["units"] if u["status"] == "covered" and u.get("note")]
    add("")
    add("## Hardening notes")
    add("")
    if not notes:
        add("None recorded.")
    else:
        for cls, note in notes:
            add("- %s: %s" % (cls, note))
    if judged_all:
        add("")
        add("## Lines judged")
        add("")
        add("Every line the tool showed and how the reviewer settled it. `safe` names the check or fact; `hardening` a change worth making without a reachable attack; `blocked` what could not be read or run.")
        for u in coverage["units"]:
            rows = [(hid, rel, number, shape, text) for hid, rel, number, shape, text in hit_rows(state, u) if hid in judged_all]
            if not rows:
                continue
            add("")
            add("### %s %s" % (u["id"], u["class"]))
            add("")
            add("| Line | Where | Shape | Verdict | Reason |")
            add("|---|---|---|---|---|")
            for hid, rel, number, shape, text in rows:
                st = judged_all[hid]
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
        add("| %s | %s | %s | %d | %s | %s |" % (u["id"], u["class"], md_escape(u["reason"]), u.get("hits") or 0, u["status"], md_escape(unit_note(u))))
    if rejected:
        add("")
        add("## Rejected claims")
        add("")
        add("Checked and disproved; kept so the next audit does not repeat them.")
        add("")
        for f in rejected:
            add("- `%s` %s: %s" % (f["id"], f["title"], f.get("reason") or ""))
    add("")
    os.makedirs(state.out, exist_ok=True)
    with open(state.path("REPORT.md"), "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))
    report_path = state.path("REPORT.md")
    finished = dict(audit, units=len(coverage["units"]), lines_judged=len(judged_all), findings=len(confirmed), leads=len(leads), hardening=sum(1 for s in judged_all.values() if s.get("status") == "hardening"), report_path=report_path, finished_ms=int(time.time() * 1000))
    state.put_audit(finished)
    print("Wrote %s; the findings are in the project's graph (the Security tab lists them by priority)." % report_path)
    watched = ensure_watch(state)
    if watched:
        print(watched)
    print(summary_line)
    for f in confirmed:
        print("  %s: %s (%s:%s). Fix: %s" % (f.get("severity_overall"), f["title"], f.get("sink_path") or "", f.get("sink_line") or 0, f.get("fix_change", "")))
    for f in leads:
        print("  needs validation: %s (%s:%s); missing: %s" % (f["title"], f.get("sink_path") or "", f.get("sink_line") or 0, f.get("blocker") or f.get("missing_fact") or ""))
    for u in blocked + left:
        print("  not covered: %s %s%s" % (u["id"], u["class"], (": " + u["note"]) if u.get("note") else ""))
    print("The report is at %s." % report_path)


# ----------------------------------------------------------------------------- scan

def changed_files(root, ref):
    """Files changed against `ref` (default HEAD), plus untracked ones; None outside a repository."""
    out = git(["diff", "--name-only", "--diff-filter=ACMR", ref or "HEAD"], root)
    if out is None:
        return None
    files = [rel for rel in out.split("\n") if rel.strip()]
    untracked = git(["ls-files", "--others", "--exclude-standard"], root) or ""
    files.extend(rel for rel in untracked.split("\n") if rel.strip())
    return sorted(set(files))


def cmd_scan(args):
    positional, flags = parse_flags(args, {"changed": "one", "classes": "one"})
    if flags.get("json"):
        return scan_json(positional, flags)
    root = os.getcwd()
    if "changed" in flags:
        ref = flags["changed"] if isinstance(flags["changed"], str) else None
        rels = changed_files(root, ref)
        if rels is None:
            fail("not a git repository, or %s is not a ref" % (ref or "HEAD"), "%s scan <path>..." % tool())
        label = "changed against %s" % (ref or "HEAD")
    else:
        if not positional:
            fail("usage: scan <path>... | scan --changed [<ref>]", "%s scan --changed" % tool())
        rels = []
        for p in positional:
            full = os.path.join(root, p)
            if os.path.isdir(full):
                rels.extend(repo_files(root, [p.rstrip("/")]))
                rels.extend(getattr(repo_files, "agent_files", []))
            elif os.path.isfile(full):
                rels.append(os.path.relpath(full, root))
            else:
                fail("%s does not exist" % p, "name repository paths")
        label = ", ".join(positional)
    agents = [rel for rel in rels if agent_file_kind(rel)]
    rels = [rel for rel in sorted(set(rels)) if not skipped(rel) and os.path.isfile(os.path.join(root, rel))]
    if not rels and not agents:
        print("Nothing to scan (%s)." % label)
        return
    catalog = Catalog()
    texts = {}
    for rel in rels:
        text = read_text(os.path.join(root, rel))
        if text is not None:
            texts[rel] = (lang_of(rel), text)
    hits = scan_texts(catalog, texts)
    for rel, number, shape, text, _ in agent_hits(root, agents):
        hits.setdefault("ai-llm-agent", []).append((rel, number, shape, text))
    print("%d files (%s)." % (len(texts), label))
    if not hits:
        print("No suspicious lines matched any class's shapes. Still read each change for: a new handler without an authorization check, a request field reaching old code, a removed condition, a new dependency, a changed default.")
        return
    for cls in catalog.classes:
        rows = hits.get(cls["id"])
        if not rows:
            continue
        print("")
        print("%s: %s" % (cls["name"], cls["counts"]))
        for rel, number, name, text in rows[:HITS_SHOWN]:
            print("  %s%s  [%s]  %s" % (rel, ":%d" % number if number else "", name, shown(text, 100)))
        if len(rows) > HITS_SHOWN:
            print("  ... %d more" % (len(rows) - HITS_SHOWN))
    print("")
    print("For each line: where does the value come from (read graph://used-by/<function>, read <path>:<from>-<to>), who controls it, which check stops it. A hit is a question, not a finding.")


def scan_json(paths, flags):
    """`scan --json`: the shapes in the named files (absolute paths; a folder is walked), as one
    JSON object. Place markers (a handler, a registration) are left out: they say where to read."""
    if not paths:
        return json_out({"error": "usage: scan --json <absolute path>... [--classes a,b]"}, 2)
    catalog = Catalog()
    wanted = None
    if isinstance(flags.get("classes"), str):
        wanted = {c.strip() for c in flags["classes"].split(",") if c.strip()}
        unknown = sorted(wanted - set(cls["id"] for cls in catalog.classes))
        if unknown:
            return json_out({"error": "unknown classes: %s (known: %s)" % (", ".join(unknown), ", ".join(cls["id"] for cls in catalog.classes))}, 2)
    files, skipped_files = [], 0
    for path in paths:
        if not os.path.isabs(path):
            return json_out({"error": "%s is not an absolute path" % path}, 2)
        if os.path.isdir(path):
            for rel in repo_files(path, []) + [r for r in getattr(repo_files, "agent_files", [])]:
                files.append(os.path.join(path, rel))
        elif os.path.isfile(path):
            files.append(path)
        else:
            return json_out({"error": "%s does not exist" % path}, 2)
    out_hits, cache = [], {}
    for full in sorted(set(files)):
        try:
            too_big = os.path.getsize(full) > MAX_FILE_BYTES
        except OSError:
            too_big = True
        text = None if too_big else read_text(full)
        if text is None:
            skipped_files += 1
            continue
        for cid, _, number, name, line in scan_lines(catalog, full, lang_of(full), enumerate(text.split("\n"), 1), wanted, False, cache):
            out_hits.append({"class": cid, "path": full, "line": number, "text": shown(line), "why": name})
        if agent_file_kind(full.replace(os.sep, "/")) and (wanted is None or "ai-llm-agent" in wanted):
            hits = agent_config_hits(full, text) if agent_file_kind(full) == "config" else []
            hits += [h + (None,) for h in hidden_text_hits(full, text)]
            for rel, number, shape, why_text, _ in hits:
                out_hits.append({"class": "ai-llm-agent", "path": full, "line": number, "text": why_text, "why": shape})
    out = {"hits": out_hits, "checked": sorted(wanted) if wanted is not None else [cls["id"] for cls in catalog.classes]}
    if skipped_files:
        out["skipped"] = "%d file%s not read: binary, unreadable or larger than %d bytes" % (skipped_files, "" if skipped_files == 1 else "s", MAX_FILE_BYTES)
    return json_out(out, 0)


# ----------------------------------------------------------------------------- the turn's own changes

def added_lines(root):
    """{rel: set of added or changed line numbers, or None for a whole new file} of the working tree
    against HEAD with the untracked files; None outside a git repository."""
    if git(["rev-parse", "--is-inside-work-tree"], root) is None:
        return None
    out = {}
    if git(["rev-parse", "--verify", "-q", "HEAD"], root) is None:
        listed = git(["ls-files", "-co", "--exclude-standard"], root) or ""
        return {rel: None for rel in listed.split("\n") if rel.strip()}
    diff = git(["diff", "HEAD", "--relative", "--unified=0", "--no-color", "--no-ext-diff", "--no-renames", "--diff-filter=AM", "--", "."], root) or ""
    current = None
    for line in diff.split("\n"):
        if line.startswith("+++ "):
            current = line[6:] if line.startswith("+++ b/") else None
            if current is not None:
                out.setdefault(current, set())
        elif line.startswith("@@") and current is not None:
            match = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", line)
            if match:
                start, count = int(match.group(1)), int(match.group(2) if match.group(2) is not None else 1)
                out[current].update(range(start, start + count))
    untracked = git(["ls-files", "--others", "--exclude-standard"], root) or ""
    for rel in untracked.split("\n"):
        if rel.strip():
            out[rel] = None
    return out


def changed_packages(root, changes):
    """{(eco, name, version): lockfile} added or changed by the working tree in its lockfiles and in
    the exact pins of its manifests."""
    added = {}
    for rel in sorted(changes):
        base = os.path.basename(rel)
        is_lock = base in LOCKFILES or base in ("go.mod", "pom.xml") or (base.startswith("requirements") and base.endswith(".txt"))
        if not is_lock and base != "package.json":
            continue
        new_text = read_text(os.path.join(root, rel))
        if new_text is None:
            continue
        old_text = git(["show", "HEAD:./" + rel], root) if changes[rel] is not None else ""
        reader = (lambda r, t: parse_lockfile(r, t)) if is_lock else manifest_pins
        try:
            new = set(reader(rel, new_text))
            old = set(reader(rel, old_text or "")) if old_text else set()
        except (ValueError, AttributeError, TypeError, IndexError):
            continue
        for item in sorted(new - old):
            added.setdefault(item, rel)
    return added


# `check-change` found something (the chat flow reads only this code as findings).
CHANGE_FOUND = 3


def cmd_check_change(args):
    """The turn's own edits: the shapes on added or changed lines, the agent-file checks on changed
    lines, and the known vulnerabilities of dependencies the change added. Silent and 0 when clean."""
    positional, flags = parse_flags(args, {"root": "one"})
    root = os.path.abspath(flags["root"]) if isinstance(flags.get("root"), str) else os.getcwd()
    if not os.path.isdir(root):
        fail("%s is not a folder" % root, "%s check-change --root <project folder>" % tool())
    changes = added_lines(root)
    if not changes:
        return
    catalog = Catalog()
    cache, lines_out = {}, []
    for rel in sorted(changes):
        full = os.path.join(root, rel)
        wanted = changes[rel]
        kind = agent_file_kind(rel)
        if (skipped(rel) and not kind) or not os.path.isfile(full) or os.path.islink(full):
            continue
        try:
            if os.path.getsize(full) > MAX_FILE_BYTES:
                continue
        except OSError:
            continue
        text = read_text(full)
        if text is None:
            continue
        numbered = [(n, line) for n, line in enumerate(text.split("\n"), 1) if wanted is None or n in wanted]
        if not skipped(rel):
            for cid, _, number, name, line in scan_lines(catalog, rel, lang_of(rel), numbered, None, False, cache):
                lines_out.append("%s:%d %s: %s (%s)" % (rel, number, cid, name, shown(line, 100)))
        if kind:
            hits = agent_config_hits(rel, text) if kind == "config" else []
            hits += [h + (None,) for h in hidden_text_hits(rel, text)]
            for _, number, shape, why, _ in hits:
                if wanted is None or number in wanted or number == 0:
                    lines_out.append("%s:%d ai-llm-agent: %s (%s)" % (rel, number, shape, why))
    deps_out = []
    added = changed_packages(root, changes)
    if added and not offline_here(root):
        try:
            rows = osv_check(added)
        except LIVE_ERRORS:
            rows = []
        for (eco, name, version, lockfile), group in group_rows(rows):
            fix = covering_fix(group)
            ids = sorted({i for r in group for i in [r["id"]] + r["aliases"]})
            deps_out.append("dependency %s@%s: %s (%s%s)" % (name, version, ", ".join(ids[:4]) + (" and %d more" % (len(ids) - 4) if len(ids) > 4 else ""), group[0]["severity"], ", fixed in %s" % fix if fix else ", no fixed version"))
    if not lines_out and not deps_out:
        return
    for line in lines_out[:CHANGE_LINES_SHOWN]:
        print(line)
    if len(lines_out) > CHANGE_LINES_SHOWN:
        print("... %d more lines like these (all of them: %s scan --changed)" % (len(lines_out) - CHANGE_LINES_SHOWN, tool()))
    for line in deps_out:
        print(line)
    print("Fix these at the sink, or say why each is safe in your reply.")
    # Its own exit code: a usage error or a crash (exit 1) is never read as a finding.
    sys.exit(CHANGE_FOUND)


# ----------------------------------------------------------------------------- the dependency watch

def cmd_watch(args):
    """Re-check the project's locked dependencies (OSV, CISA KEV, FIRST EPSS) and record what is
    vulnerable as findings (source `watch`); print only the advisories not seen by the last watch."""
    positional, flags = parse_flags(args, {"root": "one"})
    root = os.path.abspath(flags["root"]) if isinstance(flags.get("root"), str) else os.getcwd()
    if not os.path.isdir(root):
        fail("%s is not a folder" % root, "%s watch --root <project folder>" % tool())
    if offline_here(root):
        print("Offline project: the dependency watch did not run.")
        return
    pid, why = project_id(root)
    if not pid:
        fail(APP_AWAY % why, APP_NEXT)
    repo_files(root, [])
    rows, packages, not_parsed, why, problems = live_deps(root)
    if why:
        fail(why[0].upper() + why[1:] + ".", "run the watch again when the network is back")
    ok, result = graph_call("memory.security.findings", {"project_id": pid, "source": "watch", "status": ["open", "fixing", "accepted", "fixed", "verified", "gone"]})
    if not ok:
        fail("the earlier watch could not be read from the project's graph: %s" % result, APP_NEXT)
    known = {f["id"]: f for f in (result or {}).get("findings") or []}
    findings, new_lines = [], []
    for (eco, name, version, lockfile), group in group_rows(rows):
        fid = ("watch:%s:%s" % (lockfile, name)).lower()
        seen = set((known.get(fid) or {}).get("advisories") or [])
        if (known.get(fid) or {}).get("sink_version") not in (None, version):
            seen = set()
        lock_text = read_text(os.path.join(root, lockfile)) or ""
        line = pin_line(lock_text, name, version) or 0
        worst = group[0]["severity"]
        fix = quick_fix_for(name, rows, lockfile)
        ids = sorted({r["id"] for r in group})
        kev = any(r.get("kev") for r in group)
        epss = max((r.get("epss") or 0 for r in group), default=0) or None
        finding = {
            "id": fid, "class": "dependencies", "verdict": "needs_validation", "source": "watch",
            "title": "%s %s has %d known vulnerabilit%s (worst %s)" % (name, version, len(group), "y" if len(group) == 1 else "ies", worst),
            "principal": "whoever can reach the vulnerable code of %s with their own input" % name,
            "control": "", "impact": group[0]["summary"], "result": group[0]["summary"],
            "evidence_how": "the dependency watch: OSV, CISA KEV and FIRST EPSS, checked %s" % time.strftime("%Y-%m-%d %H:%M"),
            "evidence_observed": "; ".join("%s %s: %s" % (r["id"], r["severity"], r["summary"][:80]) for r in group[:4]),
            "missing_fact": "Whether this project calls the vulnerable code of %s with outside data. Check: search the code for its imports and read the advisory's affected functions" % name,
            "blocker": "whether this project calls the vulnerable code of %s with outside data" % name,
            "check_how": "search the code for its imports and read the advisory's affected functions",
            "severity_overall": worst if worst in PRIORITY_BAND else None, "host_severity": worst if worst in PRIORITY_BAND else None,
            "fix_where": lockfile, "fix_change": ("upgrade %s to %s" % (name, fix["to"])) if fix else "no fixed version is published yet", "fix_test": "the project's tests",
            "trace": [{"kind": "sink", "file": lockfile, "line": line, "what": "%s pinned at %s" % (name, version)}],
            "quick_fix": fix, "advisories": ids, "kev": kev, "epss": epss,
            "epss_percentile": max((r.get("epss_percentile") or 0 for r in group), default=0) or None,
            "signal": "malicious package" if any(i.startswith("MAL-") for i in ids) else ("exploited in the wild (CISA KEV)" if kev else None),
            "sink_path": lockfile, "sink_line": line or None, "sink_package": name, "sink_version": version,
        }
        if finding["signal"] == "malicious package":
            finding["severity_overall"] = finding["host_severity"] = "critical"
        finding["priority"], finding["priority_reason"] = priority_of(finding)
        findings.append(finding)
        for row in group:
            if row["id"] not in seen:
                extra = exploit_words([row])
                new_lines.append("%s %s %s (%s): %s%s %s%s: %s; %s" % (eco, name, version, lockfile, row["id"], (" " + row["aliases"][0]) if row["aliases"] else "", row["severity"], (", " + ", ".join(extra)) if extra else "", row["summary"][:100], ("fixed in %s" % fix["to"]) if fix else "no fixed version"))
    # A command's calls stay in its own working tree; another root is read from the project's.
    here = {"workspace": root} if os.path.realpath(root) == os.path.realpath(os.getcwd()) else {}
    if findings:
        write("memory.security.findings.put", dict({"project_id": pid, "audit_id": None, "findings": findings}, **here), "The watch's findings")
    current = {item["id"] for item in findings}
    gone = [fid for fid, item in known.items() if fid not in current and item.get("status") in ("open", "fixing", "fixed")]
    if gone:
        write("memory.security.recheck", dict({"project_id": pid, "ids": gone}, **here), "The re-check of fixed dependencies")
    for line in not_parsed:
        new_lines.append("Not read: %s." % line)
    for line in new_lines:
        print(line)
    if new_lines and findings:
        print("Next: open the Security tab to fix them; each has its upgrade there.")


# ----------------------------------------------------------------------------- next and settle: the script drives

HIT_STATUSES = ("safe", "hardening", "lead", "finding", "blocked", "fixed", "still")
HITS_PER_STEP = 30
DEPS_PER_STEP = 60


def hit_rows(state, unit):
    """The unit's hits as the model settles them: [(id, file, line, shape, text)]. A unit without
    shape hits has one hit, the unit itself (`<unit>.0`)."""
    hits = state.hits.get(unit["id"].upper()) or []
    rows = [(h["id"], h.get("path") or "", int(h.get("line") or 0), h.get("shape") or "", h.get("text") or "") for h in hits]
    if not rows:
        return [("%s.0" % unit["id"], "", 0, "the unit", "the unit's lines are not in the graph: settle the unit blocked with that reason, or run recon again")]
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
    """Every settled line: {hit id: {status, note, record, cited, finding_id}}."""
    out = {}
    for hits in state.hits.values():
        for hit in hits:
            if hit.get("verdict"):
                out[hit["id"]] = {"status": hit["verdict"], "note": hit.get("note") or "", "record": hit.get("record"), "cited": hit.get("cited"), "finding_id": hit.get("finding_id")}
    return out


def settle_hits(state, changes):
    """Write how lines were settled: {hit id: {status, note, record?, cited?, finding_id?}}."""
    now = int(time.time() * 1000)
    hits = []
    for hid, st in changes.items():
        hit = dict(state.by_hit.get(hid) or {"id": hid, "unit": hid.split(".")[0]})
        hit.update({"verdict": st["status"], "note": st.get("note", ""), "record": st.get("record"), "cited": st.get("cited"), "finding_id": st.get("finding_id") or hit.get("finding_id"), "settled_ms": now})
        hits.append(hit)
    state.put_hits(hits)


def report_current(state):
    """The report stands: written after the last settle and the last record."""
    audit = state.audit or {}
    finished = audit.get("finished_ms")
    if not finished or not audit.get("report_path") or not os.path.isfile(audit["report_path"]):
        return False
    newest = max([h.get("settled_ms") or 0 for hits in state.hits.values() for h in hits] or [0])
    if newest > finished:
        return False
    return all((f.get("last_seen_ms") or 0) <= finished + 1000 for f in state.findings())


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
    """Every unit's status and note from its hits (the notes of safe, hardening and blocked hits),
    in memory: agents settle units at the same time, so the units are not rewritten here."""
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


def pending_records(state):
    """Lines settled finding or lead whose record is not in the graph yet: [(draft name, hit id)]."""
    out = []
    for hits in state.hits.values():
        for hit in hits:
            if hit.get("verdict") in ("finding", "lead") and not hit.get("finding_id"):
                out.append((hit.get("record") or "finding-for-%s.json" % hit["id"].replace(".", "-"), hit["id"]))
    return out


def write_draft(state, unit, hid, rel, number, text, status, name=None):
    """The record template of a line settled finding or lead, prefilled with what the host knows."""
    os.makedirs(state.drafts, exist_ok=True)
    if name is None:
        n = 1
        while os.path.exists(os.path.join(state.drafts, "finding-%d.json" % n)):
            n += 1
        name = "finding-%d.json" % n
    record = json.loads(json.dumps(TEMPLATES["confirmed" if status == "finding" else "needs_validation"]))
    record["class"] = unit["class"]
    record["unit"] = unit["id"]
    record["hit"] = hid
    record["audit"] = state.audit_id
    record["fingerprint"] = "? %s:%s:<function>" % (unit["class"], rel or "<file>")
    hit = state.by_hit.get(hid) or {}
    if unit["class"] == "dependencies" and hit.get("package"):
        package = hit["package"]
        record["fingerprint"] = ("dependencies:%s:%s" % (rel, package)).lower()
        record["title"] = "? what a lower-trust party can do through %s at the pinned version" % package
        record["trace"][-1] = {"kind": "sink", "file": rel, "line": number or 1, "what": "? the package at its pinned version, and the call in this project that reaches the vulnerable code"}
    elif rel and number:
        record["trace"][-1] = {"kind": "sink", "file": rel, "line": number, "what": "? what the value does here: " + text[:80]}
    with open(os.path.join(state.drafts, name), "w", encoding="utf-8") as handle:
        json.dump(record, handle, indent=1)
    return name


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
    shown_count = DEPS_PER_STEP if unit["class"] == "dependencies" else HITS_PER_STEP
    # A file's run of lines with the same shape is one entry with an id range: one judgment
    # settles them together (seen: 61 innerHTML lines of two static design files judged one by one).
    entries = []
    index = 0
    while index < len(open_rows):
        hid, rel, number, shape, text = open_rows[index]
        run = [open_rows[index]]
        while index + len(run) < len(open_rows) and open_rows[index + len(run)][1] == rel and open_rows[index + len(run)][3] == shape:
            run.append(open_rows[index + len(run)])
        if len(run) >= 3 and not any((state.by_hit.get(r[0]) or {}).get("signal") for r in run):
            entries.append(("%s-%s" % (run[0][0], run[-1][0]), rel, 0, shape, "%d lines, e.g. :%d %s" % (len(run), number, text[:110])))
        else:
            entries.extend(run)
        index += len(run)
    strong = [e[0] for e in entries[:shown_count] if (state.by_hit.get(e[0]) or {}).get("signal")]
    print("Lines to judge now:")
    for hid, rel, number, shape, text in entries[:shown_count]:
        where = ("%s:%d" % (rel, number)) if number else rel
        mark = "  (host signal: %s)" % state.by_hit[hid]["signal"] if hid in strong else ""
        print("  %s  %s  [%s]  %s%s" % (hid, where, shape, text[:150], mark))
    if len(entries) > shown_count:
        print("  ... %d more come with the next `next` once these are settled (one control that covers a whole file or the unit settles them together: settle %s safe \"<the control>\")" % (len(entries) - shown_count, unit["id"]))
    if any("-" in e[0] for e in entries[:shown_count]):
        print("  A range (U3.4-U3.20) is one file's lines of one shape: settle the range with one reason, or a single id inside it.")
    if strong:
        print("  A line with a host signal is a real credential, a malicious or an exploited package: settle it finding or lead.")
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
    positional, flags = parse_flags(args, {"unit": "one"})
    only = (flags.get("unit") or "").upper() if isinstance(flags.get("unit"), str) else None
    root = os.getcwd()
    state = State(root)
    catalog = Catalog()
    # An audit node without units is an earlier version's record: a new run maps the project.
    fresh = state.audit is None or not state.units or (only is None and report_current(state))
    if fresh:
        if only:
            fail("no audit here yet", "%s units" % tool())
        # No audit, or the last one is complete: this is a new run (U0 holds what is still known).
        cmd_recon([])
        state.reload()
        print("")
    coverage = state.coverage()
    pending = pending_records(state)
    if only:
        # Another agent's unfilled record is not this unit's business.
        pending = [(name, hid) for name, hid in pending if hid.split(".")[0] == only]
    if pending:
        name, hid = pending[0]
        draft = os.path.join(state.drafts, name)
        if not os.path.isfile(draft):
            # The project's tmp/ was cleaned: the draft comes back from what the graph knows.
            unit = state.unit(coverage, hid.split(".")[0])
            row = next((r for r in hit_rows(state, unit) if r[0] == hid), (hid, "", 0, "", ""))
            write_draft(state, unit, hid, row[1], row[2], row[4], state.by_hit.get(hid, {}).get("verdict") or "finding", name)
        print("The record for %s is not written yet: %s still holds its template." % (hid, os.path.join(state.drafts_rel, name)))
        print("Next: fill every '? ' value with edit, then %s finding %s" % (tool(), os.path.join(state.drafts_rel, name)))
        sys.exit(1)
    states = hit_states(state)
    if only:
        # One unit, for an agent that has this unit only: shown while it has open lines, its one
        # line when it is settled.
        unit = state.unit(coverage, only)
        if not any(r[0] not in states for r in hit_rows(state, unit)):
            report_path = state.path(os.path.join("units", unit_report(state, coverage, unit, catalog)))
            print("%s is settled: %s" % (unit["id"], unit_line(state, unit, catalog, report_path)))
            print("Next: reply with that one line; this unit is done.")
            return
        first_time = not any(r[0] in states for r in hit_rows(state, unit))
        print_unit_step(state, coverage, unit, catalog, first_time)
        return
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
                state.put_units([unit])
            print_unit_step(state, coverage, unit, catalog, first_time)
        except SystemExit:
            raise
        except Exception as error:  # noqa: BLE001 - any failure of one unit's step
            message = "%s: %s" % (type(error).__name__, str(error)[:200])
            failed_before = unit.get("error")
            unit["error"] = message
            state.put_units([unit])
            print("")
            if failed_before or position == len(open_units) - 1:
                print("%s %s could not be shown%s: %s" % (unit["id"], unit["class"], " again" if failed_before else "", message))
                print("The unit stays open until its lines are settled; the report lists it as not assessed with this reason.")
                print("Next: %s settle %s blocked \"the tool failed on this unit: %s\"" % (tool(), unit["id"], message.replace('"', "'")))
                sys.exit(1)
            print("%s %s could not be shown (%s); it stays open and comes back after the other units." % (unit["id"], unit["class"], message))
            print("")
            continue
        if unit.get("error"):
            unit["error"] = None
            state.put_units([unit])
        return
    if not report_current(state):
        print("Every line of every unit is settled. Writing the report.")
        cmd_report([])
        print("")
        print("Next: %s next" % tool())
        return
    report_path = state.audit["report_path"]
    print("The audit is complete. The report (%s):" % report_path)
    print("")
    with open(report_path, encoding="utf-8") as handle:
        print(handle.read())
    print("Next: reply to the user with the report's summary, every confirmed finding (severity, title, path:line, result, fix), every lead with its missing fact, what was not covered, and the report's path %s." % report_path)


def cmd_units(args):
    """The open units, one per line as `<id>\t<title>`: what agents can take on at the same time
    (each with `next --unit <id>`). Maps the project first when no audit is open."""
    state = State(os.getcwd())
    if state.audit is None or not state.units or report_current(state):
        # The map's own account goes to stderr: stdout is the list alone.
        import contextlib
        with contextlib.redirect_stdout(sys.stderr):
            cmd_recon([])
        state.reload()
    coverage = state.coverage()
    states = hit_states(state)
    for unit in coverage["units"]:
        rows = hit_rows(state, unit)
        left = sum(1 for r in rows if r[0] not in states)
        if left:
            print("%s\t%s (%s)" % (unit["id"], unit["name"], unit["reason"]))


def fixture_proof(state, hit, cited):
    """Whether a blocked judgment of a strong-signal line rests on a test fixture: the cited place is
    the line itself, in a test folder or file, and its value is a known example or placeholder."""
    if not cited or not hit.get("path") or not hit.get("line"):
        return False
    if cited != "%s:%d" % (hit["path"], hit["line"]) or not TEST_PLACE.search(hit["path"]):
        return False
    lines = (read_text(os.path.join(state.root, hit["path"])) or "").split("\n")
    raw = lines[hit["line"] - 1] if 0 < hit["line"] <= len(lines) else ""
    values = [m.group(0) for regex in SECRET_VALUE_RES for m in regex.finditer(raw)]
    if "PRIVATE KEY" in raw and not values:
        return bool(EXAMPLE_VALUE.search(os.path.basename(hit["path"])))
    return bool(values) and all(EXAMPLE_VALUE.search(v) or len(set(v[4:])) <= 2 for v in values)


def cmd_settle(args):
    if len(args) < 2:
        fail("usage: settle <id or unit>... safe|hardening|lead|finding|blocked [\"note\"]", "%s next" % tool())
    ids, status, note = [], None, ""
    for index, arg in enumerate(args):
        if arg in HIT_STATUSES:
            status, note = arg, " ".join(args[index + 1:]).strip().strip('"')
            break
        ids.append(arg.upper())
    if status is None or not ids:
        fail("usage: settle <id or unit>... safe|hardening|lead|finding|blocked [\"note\"]", "%s next" % tool())
    if status in ("fixed", "still") and not all(i.startswith("U0") for i in ids):
        fail("fixed and still are for the issues already known (U0) only", "%s next" % tool())
    if status in ("safe", "hardening", "blocked", "fixed", "still") and len(note) < 8:
        fail("%s needs the reason (the check or fact, with path:line)" % status, "%s settle %s %s \"<reason>\"" % (tool(), " ".join(ids), status))
    state = State(os.getcwd())
    coverage = state.coverage()
    catalog = Catalog()
    states = hit_states(state)
    units = {u["id"]: u for u in coverage["units"]}
    targets = []
    for ident in ids:
        unit_id = ident.split(".")[0].split("-")[0]
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
    elif status in ("safe", "hardening", "blocked") and (len(targets) > 1 or any((state.by_hit.get(r[0]) or {}).get("signal") for _, r in targets)):
        cited = cited_location(state, note, targets)
        if cited is None and len(targets) > 1:
            first = targets[0][1]
            example = ("%s:%d" % (first[1], first[2])) if first[1] and first[2] else (first[1] or "src/app.py:42")
            if targets[0][0]["class"] == "dependencies":
                fail("a judgment of %d packages names the package or lockfile it rests on" % len(targets), "%s settle %s %s \"<reason naming the package>\"" % (tool(), " ".join(ids), status))
            fail("a judgment of %d lines names the place you read that covers them, as path:line in the project (e.g. %s)" % (len(targets), example), "%s settle %s %s \"<reason with path:line>\"" % (tool(), " ".join(ids), status))
    # A strong host signal (a real key format, a secret in history, a malicious or exploited
    # package) is never waved off: finding or lead, or blocked on a cited test fixture.
    if status in ("safe", "hardening", "blocked"):
        for unit, row in targets:
            hit = state.by_hit.get(row[0]) or {}
            if not hit.get("signal"):
                continue
            if status == "blocked" and fixture_proof(state, hit, cited):
                continue
            proof = " (blocked needs the cited path:line of a test fixture holding an example value)" if status == "blocked" else ""
            fail("%s is a strong host signal (%s): settle it finding or lead, not %s%s." % (row[0], hit["signal"], status, proof), "%s settle %s finding" % (tool(), row[0]))
    changes, written = {}, []
    for unit, (hid, rel, number, shape, text) in targets:
        hit = state.by_hit.get(hid) or {}
        if unit["class"] == "previous" and status in ("fixed", "still"):
            fid = hit.get("finding_id")
            if not fid:
                changes[hid] = {"status": "hardening" if status == "still" else "fixed", "note": ("still open: " if status == "still" else "") + note}
                continue
            ok, result = graph_call("memory.security.finding", {"project_id": state.pid, "id": fid})
            if not ok:
                fail("finding %s could not be read from the project's graph: %s" % (fid, result), "%s next" % tool())
            finding = (result or {}).get("finding") or {}
            if status == "fixed":
                if finding.get("status") not in ("fixed", "verified"):
                    kind, words, _ = now_at_sink(state.pid, state.root, fid)
                    if kind != "gone":
                        fail("%s is not fixed: %s." % (hid, words), "%s settle %s still \"<re-verified how>\"" % (tool(), hid))
                    if finding.get("status") == "accepted":
                        print("%s stays accepted (the person's decision); its line is gone." % fid)
                    else:
                        write("memory.security.finding.update", {"project_id": state.pid, "id": fid, "status": "fixed", "by": "recheck", "note": note, "workspace": state.root}, "The fix of %s" % fid)
                changes[hid] = {"status": "fixed", "note": note, "finding_id": fid}
            else:
                again = {k: v for k, v in finding.items() if k not in ("status", "status_note", "status_by", "status_ms", "notified_ms", "ended_ms")}
                again["priority"], again["priority_reason"] = priority_of(again)
                state.put_findings([again])
                carried = {"confirmed": "finding", "needs_validation": "lead"}.get(finding.get("verdict"), "hardening")
                changes[hid] = {"status": carried, "note": "still open: " + note, "finding_id": fid, "record": "carried"}
            continue
        if status in ("finding", "lead"):
            name = write_draft(state, unit, hid, rel, number, text, status)
            written.append((hid, os.path.join(state.drafts_rel, name)))
            changes[hid] = {"status": status, "note": note, "record": name}
        else:
            changes[hid] = {"status": status, "note": note}
            if cited:
                changes[hid]["cited"] = cited
    settle_hits(state, changes)
    coverage = state.coverage()
    closed = []
    for unit in {u["id"]: u for u, _ in targets}.values():
        current = next(u for u in coverage["units"] if u["id"] == unit["id"])
        if current["status"] in ("covered", "candidate", "blocked"):
            closed.append((current, state.path(os.path.join("units", unit_report(state, coverage, current, catalog)))))
    print("Settled %s as %s." % (", ".join(hid for _, (hid, *_) in targets), status))
    if written:
        for hid, path in written:
            print("Record for %s: %s" % (hid, path))
            floor = (state.by_hit.get(hid) or {}).get("severity")
            if floor:
                print("The host sets %s for this line's signal: the severity is at least %s." % (floor, floor))
        if status == "finding":
            print("Severity comes from the demonstrated result, never from the class name; overall never exceeds impact:")
            for anchor in SEVERITY_ANCHORS:
                print("  " + anchor)
        print("Next: fill every '? ' value of %s with edit (trace: entrypoint first, the sink is prefilled; a real path:line each), then %s finding %s" % (written[0][1], tool(), written[0][1]))
        return
    states = hit_states(state)
    still_open = any(r[0] not in states for u in coverage["units"] for r in hit_rows(state, u))
    if closed and still_open:
        # A unit is one piece of the work: its one-line result goes to the user now, and the audit
        # goes on with the next unit from the graph (the host continues the turn while the tool
        # says work is open), so each unit starts from a short history.
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
    """Write `units/<id>-<class>.md` in the audit's report folder: the unit's review in full, as a
    reader would want it: what the class is, why it applies here, each finding and lead in prose,
    the hardening notes, and every line reviewed with its verdict and the reviewer's reasoning."""
    cls = catalog.by_id[unit["class"]]
    states = hit_states(state)
    rows = hit_rows(state, unit)
    judged = [(r, states.get(r[0])) for r in rows]
    counts = {}
    for _, st in judged:
        counts[st["status"] if st else "open"] = counts.get(st["status"] if st else "open", 0) + 1
    by_id = {f["id"]: f for f in state.findings()}
    deps = state.rows()
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
    findings = [(r, by_id[st["finding_id"]]) for r, st in judged if st and st["status"] in ("finding", "lead") and st.get("finding_id") in by_id]
    if findings:
        add("")
        add("## Findings and leads")
        for (hid, rel, number, shape, text), rec in findings:
            add("")
            trace = " then ".join("`%s:%d` (%s: %s)" % (s["file"], s["line"], s["kind"], s["what"]) for s in rec.get("trace") or [])
            if rec["verdict"] == "confirmed":
                add("### %s (%s severity)" % (rec["title"], rec.get("severity_overall")))
                add("")
                add("Affected code: " + trace + ".")
                add("")
                add("Who can exploit it: %s" % sentence(rec.get("principal")))
                add("")
                add("Why the existing controls do not stop it: %s" % sentence(rec.get("control")))
                add("")
                add("Impact: %s" % sentence(rec.get("impact")))
                if rec.get("conditions"):
                    add("")
                    add("Conditions: " + "; ".join(rec["conditions"]) + ".")
                add("")
                add("Evidence: %s Observed: %s" % (sentence(rec.get("evidence_how")), sentence(rec.get("evidence_observed"))))
                add("")
                add("Severity: likelihood %s, impact %s, overall %s." % (rec.get("severity_likelihood"), rec.get("severity_impact"), rec.get("severity_overall")))
                add("")
                add("Recommended fix: in %s, %s Regression test: %s" % (rec.get("fix_where"), sentence(rec.get("fix_change")), sentence(rec.get("fix_test"))))
            else:
                add("### %s (lead, needs validation)" % rec["title"])
                add("")
                add("Affected code: " + trace + ".")
                add("")
                add("Who could exploit it: %s" % sentence(rec.get("principal")))
                add("")
                add("The one fact outside the code that decides: %s" % sentence(rec.get("blocker") or rec.get("missing_fact")))
                add("")
                add("How to settle it without attacking anything: %s" % sentence(rec.get("check_how")))
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
            groups.setdefault((row["eco"], row["name"], row["version"]), []).append(row)
        for (hid, rel, number, shape, text), st in judged:
            hit = state.by_hit.get(hid) or {}
            add("### %s %s %s" % (hid, hit.get("package") or text.split(":")[0], hit.get("version") or ""))
            add("")
            for row in groups.get((hit.get("ecosystem"), hit.get("package"), hit.get("version")), []):
                ids = row["id"] + (" / " + row["aliases"][0] if row["aliases"] else "")
                extra = exploit_words([row])
                add("- %s, %s%s: %s Fixed in %s." % (ids, row["severity"], (" (" + ", ".join(extra) + ")") if extra else "", sentence(row["summary"]), ", ".join(sorted(set(row["fixed"]))[:3]) or "no version listed"))
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


# ----------------------------------------------------------------------------- one finding: show, fix-plan, recheck, resolve

def graph_finding(pid, ident):
    """A finding from the graph with its history: (finding, history), or (None, why)."""
    ok, result = graph_call("memory.security.finding", {"project_id": pid, "id": ident})
    if not ok:
        return None, result
    return (result or {}).get("finding"), (result or {}).get("history", [])


def now_at_sink(pid, root, ident):
    """What the host sees of a finding's sink now: (kind, sentence, line)."""
    ok, result = graph_call("memory.security.recheck", {"project_id": pid, "ids": [ident], "workspace": root, "dry": True})
    row = ((result or {}).get("checked") or [{}])[0] if ok else {}
    if not ok or not row:
        return "unknown", "the host could not compare (%s)" % (result if not ok else "no such finding"), None
    return row.get("kind", "unknown"), row.get("sentence", ""), row.get("line")


def one_finding(args, usage):
    if not args:
        fail("which finding?", "%s %s <id>" % (tool(), usage))
    root = os.getcwd()
    pid = require_app(root)
    finding, history = graph_finding(pid, args[0])
    if finding is None:
        fail("no finding %s in the project's graph (%s)" % (args[0], history), "%s status" % tool())
    return root, pid, finding, history


def cmd_show(args):
    """Print one finding, readable, for the fix session."""
    root, pid, finding, history = one_finding(args, "show")
    level = finding.get("severity_overall") or {"needs_validation": "lead", "hardening": "hardening note"}.get(finding.get("verdict"), finding.get("verdict"))
    print("%s [%s, %s] %s" % (finding["id"], level, finding.get("class", ""), finding.get("title", "")))
    if finding.get("status"):
        print("Status: %s%s." % (finding["status"], (" (" + finding.get("status_note", "") + ")") if finding.get("status_note") else ""))
    for label, key in (("Who", "principal"), ("Result", "result"), ("Control", "control"), ("Impact", "impact"), ("Missing fact", "missing_fact")):
        if finding.get(key):
            print("%s: %s" % (label, finding[key]))
    if finding.get("kev") or finding.get("epss"):
        print("Exploitation: %s." % ", ".join(w for w in [("exploited in the wild (CISA KEV)" if finding.get("kev") else ""), ("EPSS %d%%" % round(finding["epss"] * 100) if finding.get("epss") else "")] if w))
    if finding.get("trace"):
        print("Trace (entry point first, sink last):")
        for step in finding["trace"]:
            print("  %s %s:%d  %s" % (step.get("kind", "step"), step.get("file", ""), int(step.get("line") or 0), step.get("what", "")))
    if finding.get("fix_change") or finding.get("fix_where"):
        print("Fix: %s at %s" % (finding.get("fix_change", ""), finding.get("fix_where", "")))
    if finding.get("fix_test"):
        print("Test that proves it: %s" % finding["fix_test"])
    if finding.get("quick_fix"):
        qf = finding["quick_fix"]
        print("Quick fix: %s %s %s -> %s in %s:" % (qf["ecosystem"], qf["package"], qf["from"], qf["to"], qf["lockfile"]))
        for command in qf.get("commands", []):
            print("  " + command)
    kind, words, _ = now_at_sink(pid, root, finding["id"])
    print("Now: %s." % words)
    print("Next: make the fix at the sink, run the test, then %s resolve %s \"<how you verified it>\"" % (tool(), finding["id"]))


def cmd_fix_plan(args):
    """The exact commands of a quick fix."""
    root, pid, finding, _ = one_finding(args, "fix-plan")
    qf = finding.get("quick_fix")
    if not qf:
        print("%s has no quick fix: it needs a code change. Fix: %s at %s" % (finding["id"], finding.get("fix_change", ""), finding.get("fix_where", "")))
        print("Next: %s show %s" % (tool(), finding["id"]))
        return
    print("Upgrade %s from %s to %s (%s, pinned in %s):" % (qf["package"], qf["from"], qf["to"], qf["ecosystem"], qf["lockfile"]))
    for command in qf.get("commands", []):
        print("  " + command)
    print("Then run the project's tests, and: %s resolve %s \"upgraded %s to %s, tests pass\"" % (tool(), finding["id"], qf["package"], qf["to"]))


def cmd_recheck(args):
    """The host's re-check of one finding (or every open, fixing and fixed one): what the code says
    now, and the move it makes (gone: fixed; fixed and still gone later: verified; back: open)."""
    root = os.getcwd()
    pid = require_app(root)
    params = {"project_id": pid, "workspace": root}
    if args:
        params["ids"] = [args[0]]
    result = write("memory.security.recheck", params, "The re-check")
    rows = (result or {}).get("checked") or []
    if args and not rows:
        fail("no finding %s in the project's graph" % args[0], "%s status" % tool())
    for row in rows:
        print("%s: %s%s." % (row["id"], row.get("sentence", ""), ("; now %s" % row["now"]) if row.get("now") else ""))
    if not rows:
        print("No open, fixing or fixed finding to re-check.")
    print("Next: %s status" % tool())


def cmd_resolve(args):
    """Record a fix: the host checks that the sink's line (or the pinned version) is gone from the
    working tree, then the finding is fixed with how it was verified, the session and the commit."""
    if len(args) < 2 or len(" ".join(args[1:]).strip().strip('"')) < 8:
        fail("usage: resolve <id> \"<how you verified the fix: the test or run>\"", "%s show <id>" % tool())
    ident, how = args[0], " ".join(args[1:]).strip().strip('"')
    root = os.getcwd()
    pid = require_app(root)
    commit = (git(["rev-parse", "HEAD"], root) or "").strip() or None
    params = {"project_id": pid, "id": ident, "status": "fixed", "by": "agent", "note": how, "workspace": root, "commit": commit}
    session = os.environ.get("CORTEX_SESSION_ID")
    if session:
        params["session_id"] = session
    ok, result = graph_call("memory.security.finding.update", params)
    if not ok:
        print("The fix was not recorded: %s" % result)
        if "Next:" not in result:
            print("Next: %s show %s" % (tool(), ident))
        sys.exit(1)
    print("Recorded %s as fixed: %s." % (ident, (result or {}).get("status_note", how)))
    print("Next: reply to the user with one line: what was changed (path:line), how it was verified, and that the finding is marked fixed.")


# ----------------------------------------------------------------------------- main

COMMANDS = {"next": cmd_next, "units": cmd_units, "settle": cmd_settle, "recon": cmd_recon, "hunt": cmd_hunt, "cover": cmd_cover, "new": cmd_new, "finding": cmd_finding, "status": cmd_status, "report": cmd_report, "scan": cmd_scan, "deps": cmd_deps, "cve": cmd_cve, "show": cmd_show, "fix-plan": cmd_fix_plan, "recheck": cmd_recheck, "resolve": cmd_resolve, "check-change": cmd_check_change, "watch": cmd_watch}


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
    # A result piped into head must not end in a traceback; a terminal without UTF-8 gets "?" for
    # the ellipsis of a masked secret instead of an error.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(errors="replace")
        except (AttributeError, ValueError):
            pass
    sys.exit(main(sys.argv[1:]))
