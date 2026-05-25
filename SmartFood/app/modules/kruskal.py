"""Module 2 -- Arbre Couvrant de Poids Minimal (algorithme de Kruskal).

Implementation conforme au cours "Theorie des graphes" (graphes.pdf, p.20-21) :
  - On trie l'ensemble A des aretes par valuations croissantes.
  - On parcourt les aretes dans l'ordre ; si F U {e} reste acyclique, on ajoute e.
  - On termine des qu'on a selectionne n - 1 aretes.

L'acyclicite est testee avec une structure Union-Find (ensembles disjoints) :
deux sommets deja dans la meme composante -> ajouter l'arete creerait un cycle.
"""

from app.data import aretes_acpm, SITES_ACPM


class UnionFind:
    """Structure d'ensembles disjoints pour le test d'acyclicite de Kruskal."""

    def __init__(self, elements):
        self.parent = {e: e for e in elements}
        self.rang = {e: 0 for e in elements}

    def trouver(self, x):
        # Recherche du representant avec compression de chemin
        while self.parent[x] != x:
            self.parent[x] = self.parent[self.parent[x]]
            x = self.parent[x]
        return x

    def union(self, x, y):
        """Fusionne les ensembles de x et y. Retourne False si deja unis (cycle)."""
        rx, ry = self.trouver(x), self.trouver(y)
        if rx == ry:
            return False
        # Union par rang
        if self.rang[rx] < self.rang[ry]:
            rx, ry = ry, rx
        self.parent[ry] = rx
        if self.rang[rx] == self.rang[ry]:
            self.rang[rx] += 1
        return True


def kruskal(sommets, aretes):
    """Applique Kruskal.

    Retourne (arbre, poids_total, etapes) ou :
      - arbre       : liste des aretes retenues (u, v, poids)
      - poids_total : somme des valuations de l'arbre
      - etapes      : trace de chaque arete examinee {arete, acceptee, raison}
    """
    n = len(sommets)
    aretes_triees = sorted(aretes, key=lambda a: a[2])
    uf = UnionFind(sommets)
    arbre = []
    poids_total = 0
    etapes = []

    for (u, v, poids) in aretes_triees:
        if len(arbre) == n - 1:
            # L'arbre est complet : aretes restantes non examinees
            break
        acceptee = uf.union(u, v)
        if acceptee:
            arbre.append((u, v, poids))
            poids_total += poids
            raison = "ajoutee (ne cree pas de cycle)"
        else:
            raison = "rejetee (creerait un cycle)"
        etapes.append({
            "arete": f"{u} - {v}",
            "u": u,
            "v": v,
            "poids": poids,
            "acceptee": acceptee,
            "raison": raison,
            "nb_aretes_arbre": len(arbre),
        })

    return arbre, poids_total, etapes


def cout_etoile(sommets, matrice_aretes, centre="E"):
    """Cout d'une topologie en etoile : centre relie directement a tous les autres.

    Sert de comparaison avec l'ACPM (analyse de sensibilite du sujet).
    """
    couts = {}
    total = 0
    for (u, v, poids) in matrice_aretes:
        if u == centre:
            couts[v] = poids
        elif v == centre:
            couts[u] = poids
    for s in sommets:
        if s != centre:
            total += couts.get(s, 0)
    return total, couts


def solve():
    """Calcule l'ACPM du reseau de cablage fibre optique.

    Retourne un dictionnaire serialisable en JSON.
    """
    sommets = SITES_ACPM
    aretes = aretes_acpm()
    arbre, poids_total, etapes = kruskal(sommets, aretes)

    cout_etoile_val, couts_etoile = cout_etoile(sommets, aretes, centre="E")

    return {
        "sites": sommets,
        "nb_aretes_total": len(aretes),
        "arbre": [{"from": u, "to": v, "poids": p} for (u, v, p) in arbre],
        "poids_total": poids_total,
        "etapes": etapes,
        "comparaison_etoile": {
            "cout_acpm": poids_total,
            "cout_etoile": cout_etoile_val,
            "economie": cout_etoile_val - poids_total,
            "details_etoile": couts_etoile,
        },
        # Toutes les aretes (pour dessiner le graphe complet en arriere-plan)
        "aretes_completes": [{"from": u, "to": v, "poids": p}
                             for (u, v, p) in aretes],
    }


if __name__ == "__main__":
    import json
    print(json.dumps(solve(), indent=2, ensure_ascii=False))
