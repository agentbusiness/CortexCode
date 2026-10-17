import pandas as pd


def task_func(df: pd.DataFrame, column: str, threshold: float) -> pd.DataFrame:
    """
    Filter the rows of a DataFrame whose value in `column` is strictly greater than `threshold`,
    sort them by that column in descending order and reset the index.

    Parameters:
    df (pd.DataFrame): The input data.
    column (str): The column to filter and sort on.
    threshold (float): Rows with a value greater than this are kept.

    Returns:
    pd.DataFrame: The filtered, sorted DataFrame with a fresh RangeIndex.

    Raises:
    ValueError: If `df` is not a DataFrame or `column` is not one of its columns.

    Requirements:
    - pandas

    Example:
    >>> df = pd.DataFrame({'a': [1, 5, 3]})
    >>> task_func(df, 'a', 2)
       a
    0  5
    1  3
    """
    pass
