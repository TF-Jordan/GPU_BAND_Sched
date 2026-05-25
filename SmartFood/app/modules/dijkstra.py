"""Module 1 -- Plus courts chemins (algorithme de Dijkstra).

Implementation conforme au cours "Theorie des graphes" (graphes.pdf, p.16-17) :
  - Initialisation : d(x0) = 0, d(s) = +inf, P(s) = nul pour tout autre sommet.
  - A chaque etape : choisir le sommet non marque x de distance estimee minimale,
    le marquer, puis mettre a jour les distances de ses successeurs non marques.
  - On enregistre le tableau d'iterations (M | d(.) | P(.)) tel que dans le cours.
  - Le chemin se reconstruit a partir du tableau des peres P, en partant de la fin.

L'algorithme de Dijkstra est utilise car toutes les valuations sont positives
(le cours indique qu'il est "a privilegier systematiquement" dans ce cas).
"""

from app.data import (
    graphe_successeurs,
    SOMMETS_LIVRAISON,
    CLIENTS,
    ENTREPOT,
    LABO,
    CUISINES,
)

INF = float("inf")


def dijkstra(graphe, source, sommets):
    """Applique Dijkstra depuis `source`.

    Retourne (distances, peres, iterations) ou :
      - distances[s] : distance minimale source -> s (ou +inf)
      - peres[s]     : predecesseur de s sur un plus court chemin (ou None)
      - iterations   : liste d'etats {marque, distances, peres} apres chaque marquage
    """
    distances = {s: INF for s in sommets}
    peres = {s: None for s in sommets}
    distances[source] = 0
    marques = set()
    iterations = []

    # Etat initial (avant tout marquage)
    iterations.append({
        "sommet_marque": None,
        "ensemble_M": [],
        "distances": dict(distances),
        "peres": dict(peres),
    })

    while len(marques) < len(sommets):
        # Choisir le sommet non marque de distance estimee minimale
        x = None
        d_min = INF
        for s in sommets:
            if s not in marques and distances[s] < d_min:
                d_min = distances[s]
                x = s
        if x is None:
            break  # sommets restants inaccessibles (d = +inf)

        marques.add(x)

        # Mise a jour des successeurs non marques de x
        for y, poids in graphe[x].items():
            if y not in marques and distances[x] + poids < distances[y]:
                distances[y] = distances[x] + poids
                peres[y] = x

        iterations.append({
            "sommet_marque": x,
            "ensemble_M": sorted(marques),
            "distances": dict(distances),
            "peres": dict(peres),
        })

    return distances, peres, iterations


def reconstruire_chemin(peres, source, cible):
    """Reconstruit le chemin source -> cible a partir du tableau des peres.

    On part de la fin (cible) et on remonte les peres jusqu'a la source.
    Retourne la liste des sommets [source, ..., cible] ou None si pas de chemin.
    """
    if peres.get(cible) is None and cible != source:
        return None
    chemin = [cible]
    courant = cible
    while courant != source:
        courant = peres[courant]
        if courant is None:
            return None
        chemin.append(courant)
    chemin.reverse()
    return chemin


def solve():
    """Calcule les plus courts chemins E -> cuisine -> client -> Labo.

    Retourne un dictionnaire serialisable en JSON contenant :
      - arcs           : arcs du graphe (pour la visualisation)
      - distances      : distance E -> s pour chaque sommet s
      - iterations     : tableau d'iterations de Dijkstra
      - trajets        : pour chaque client, le trajet et la distance E -> Labo
      - comparaison    : tableau trie des distances E -> Labo par client
    """
    graphe = graphe_successeurs()
    distances, peres, iterations = dijkstra(graphe, ENTREPOT, SOMMETS_LIVRAISON)

    # Trajet E -> ... -> client -> Labo pour chaque client
    trajets = []
    for client in CLIENTS:
        # plus court chemin E -> client
        chemin_client = reconstruire_chemin(peres, ENTREPOT, client)
        if chemin_client is None or distances[client] == INF:
            trajets.append({
                "client": client,
                "atteignable": False,
                "chemin_complet": None,
                "distance_totale": None,
                "cuisine": None,
                "detail": f"Le client {client} n'est pas atteignable depuis "
                          f"l'entrepot E dans le graphe oriente donne.",
            })
            continue

        # Distance client -> Labo (arc direct dans ce graphe)
        dist_client_labo = graphe[client].get(LABO, INF)
        if dist_client_labo == INF:
            trajets.append({
                "client": client,
                "atteignable": False,
                "chemin_complet": None,
                "distance_totale": None,
                "cuisine": None,
                "detail": f"Aucun arc {client} -> Labo.",
            })
            continue

        chemin_complet = chemin_client + [LABO]
        distance_totale = distances[client] + dist_client_labo
        # La cuisine traversee est le 2e sommet du chemin (E -> cuisine -> ...)
        cuisine = chemin_client[1] if len(chemin_client) > 1 else None

        trajets.append({
            "client": client,
            "atteignable": True,
            "chemin_complet": chemin_complet,
            "distance_totale": distance_totale,
            "cuisine": cuisine,
            "detail": " -> ".join(chemin_complet)
                      + f" = {distance_totale} km",
        })

    # Comparaison triee (clients atteignables d'abord, par distance croissante)
    comparaison = sorted(
        trajets,
        key=lambda t: (not t["atteignable"],
                       t["distance_totale"] if t["distance_totale"] is not None else INF),
    )

    # Distances serialisables (remplacer inf par None)
    distances_json = {s: (None if d == INF else d) for s, d in distances.items()}

    return {
        "arcs": [{"from": o, "to": f, "poids": p} for o, f, p in
                 _arcs_list()],
        "sommets": SOMMETS_LIVRAISON,
        "cuisines": CUISINES,
        "clients": CLIENTS,
        "entrepot": ENTREPOT,
        "labo": LABO,
        "distances": distances_json,
        "iterations": _iterations_json(iterations),
        "trajets": trajets,
        "comparaison": comparaison,
    }


def _arcs_list():
    from app.data import ARCS_LIVRAISON
    return ARCS_LIVRAISON


def _iterations_json(iterations):
    """Convertit les +inf en None pour la serialisation JSON."""
    result = []
    for it in iterations:
        result.append({
            "sommet_marque": it["sommet_marque"],
            "ensemble_M": it["ensemble_M"],
            "distances": {s: (None if d == INF else d)
                          for s, d in it["distances"].items()},
            "peres": it["peres"],
        })
    return result


if __name__ == "__main__":
    import json
    print(json.dumps(solve(), indent=2, ensure_ascii=False))
