"""Two order workers draining the same queue file (one order per line: customer,sku,quantity)."""
import sys
from multiprocessing import Process

from .orders import place_order


def work(lines):
    for line in lines:
        customer, sku, quantity = line.strip().split(",")
        place_order(int(customer), sku, int(quantity))


def main(path="queue.txt"):
    lines = open(path).read().splitlines()
    procs = [Process(target=work, args=(lines[i::2],)) for i in range(2)]
    for p in procs:
        p.start()
    for p in procs:
        p.join()


if __name__ == "__main__":
    main(*sys.argv[1:])
