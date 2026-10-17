import re
import sys

# host ident user [time] "request" status size
LINE = re.compile(r'^(\S+) (\S+) (\S+) \[([^\]]+)\] "((?:[^"\\]|\\.|\s*\w+\s*)*)" (\d{3}) (\d+|-)$')


def parse(path):
    for line in open(path):
        m = LINE.match(line.rstrip("\n"))
        if m:
            yield {"host": m.group(1), "request": m.group(5), "status": int(m.group(6))}


if __name__ == "__main__":
    for rec in parse(sys.argv[1]):
        print(rec["status"], rec["request"])
