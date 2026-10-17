import yaml


def load(path="config.yaml"):
    with open(path) as fh:
        return yaml.safe_load(fh)


if __name__ == "__main__":
    print(load()["countries"])
