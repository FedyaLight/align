"""Path helpers shared by the acceptance scripts."""
from pathlib import Path
from urllib.parse import urlparse
from urllib.request import url2pathname


def locator_path(value):
    """Local path for a file URL as Align writes it, or a plain path unchanged.

    On Windows `file:///D:/media/a.wav` is `D:\\media\\a.wav`; decoding only the
    URL path would keep a leading slash and drop the drive.
    """
    if not value.startswith('file:'):
        return Path(value)
    return Path(url2pathname(urlparse(value).path))
