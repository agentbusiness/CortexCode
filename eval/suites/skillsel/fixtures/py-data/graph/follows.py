import csv

from neo4j import GraphDatabase


def import_follows(uri, auth, path="graph/follows.csv"):
    """Load who-follows-whom from the CSV export into Neo4j."""
    driver = GraphDatabase.driver(uri, auth=auth)
    with driver.session() as session, open(path, newline="") as fh:
        for row in csv.DictReader(fh):
            session.run(
                "MERGE (a:User {name: $a})-[:FOLLOWS]->(b:User {name: $b})",
                a=row["follower"],
                b=row["followed"],
            )
    driver.close()


def follower_counts(uri, auth):
    driver = GraphDatabase.driver(uri, auth=auth)
    with driver.session() as session:
        result = session.run("MATCH (u:User)<-[:FOLLOWS]-(f) RETURN u.name AS name, count(f) AS followers")
        counts = {r["name"]: r["followers"] for r in result}
    driver.close()
    return counts
