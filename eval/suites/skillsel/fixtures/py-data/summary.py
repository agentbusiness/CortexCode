import pandas as pd


def revenue_per_region(data_dir="data"):
    sales = pd.read_csv(f"{data_dir}/sales.csv")
    stores = pd.read_csv(f"{data_dir}/stores.csv")
    regions = pd.read_csv(f"{data_dir}/regions.csv")
    df = sales.merge(stores, on="store_id").merge(regions, on="region", how="left")
    return df.groupby(["region", "manager"], dropna=False)["amount"].sum().reset_index()


if __name__ == "__main__":
    print(revenue_per_region())
