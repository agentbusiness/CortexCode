import subprocess
from pathlib import Path

UPLOADS = Path("uploads")
THUMBS = Path("thumbs")


def make_thumbnail(filename):
    """Thumbnail of an uploaded product photo; `filename` comes from the upload form."""
    THUMBS.mkdir(exist_ok=True)
    subprocess.run(f"convert {UPLOADS}/{filename} -resize 200x200 {THUMBS}/{filename}", shell=True, check=True)
    return THUMBS / filename
