"""Donnees du projet SmartFood (extraites du sujet).

Centralise les quatre jeux de donnees utilises par les modules :
  1. Graphe oriente des distances de livraison (Dijkstra)
  2. Matrice des distances entre sites pour le cablage fibre (Kruskal / ACPM)
  3. Tableau des taches de production quotidienne (PERT)
  4. Coefficients du programme lineaire (optimisation des quantites)
"""

# ---------------------------------------------------------------------------
# 1) GRAPHE ORIENTE DES DISTANCES (livraison) -- pour Dijkstra
#    Represente comme un dictionnaire des successeurs G(s) (convention du cours).
#    G[x] = { y : v(x, y) }  ou v(x, y) est la valuation (distance en km).
# ---------------------------------------------------------------------------
ARCS_LIVRAISON = [
    ("E", "C1", 5),
    ("E", "C2", 7),
    ("C1", "A", 3),
    ("C1", "B", 4),
    ("C2", "C", 2),
    ("C2", "D", 5),
    ("C3", "F", 6),
    ("C4", "E", 8),
    ("A", "Labo", 2),
    ("B", "Labo", 3),
    ("C", "Labo", 4),
    ("D", "Labo", 1),
    ("F", "Labo", 7),
    ("C3", "C", 9),
]

# Sommets du graphe oriente
SOMMETS_LIVRAISON = ["E", "C1", "C2", "C3", "C4", "A", "B", "C", "D", "F", "Labo"]

# Clients a livrer (attention : C est un client, C1..C4 sont des cuisines)
CLIENTS = ["A", "B", "C", "D", "F"]
CUISINES = ["C1", "C2", "C3", "C4"]
ENTREPOT = "E"
LABO = "Labo"


def graphe_successeurs():
    """Construit le dictionnaire des successeurs G(s) a partir des arcs."""
    G = {s: {} for s in SOMMETS_LIVRAISON}
    for origine, fin, poids in ARCS_LIVRAISON:
        G[origine][fin] = poids
    return G


# ---------------------------------------------------------------------------
# 2) MATRICE DES DISTANCES ENTRE LES 10 SITES (non orientee) -- pour Kruskal
#    Ordre des sites : E, C1, C2, C3, C4, A, B, C, D, F
# ---------------------------------------------------------------------------
SITES_ACPM = ["E", "C1", "C2", "C3", "C4", "A", "B", "C", "D", "F"]

MATRICE_ACPM = [
    # E   C1  C2  C3  C4  A   B   C   D   F
    [0,   5,  7, 12,  9, 14, 13, 18, 20, 22],  # E
    [5,   0,  4, 10,  8,  3,  4, 11, 13, 15],  # C1
    [7,   4,  0,  6,  5,  9,  8,  2,  5, 12],  # C2
    [12, 10,  6,  0,  7, 15, 14,  9, 11,  6],  # C3
    [9,   8,  5,  7,  0, 12, 11,  4,  6, 10],  # C4
    [14,  3,  9, 15, 12,  0,  2, 10, 12, 16],  # A
    [13,  4,  8, 14, 11,  2,  0,  9, 11, 15],  # B
    [18, 11,  2,  9,  4, 10,  9,  0,  3,  8],  # C
    [20, 13,  5, 11,  6, 12, 11,  3,  0,  7],  # D
    [22, 15, 12,  6, 10, 16, 15,  8,  7,  0],  # F
]


def aretes_acpm():
    """Liste les aretes (non orientees, i<j) sous forme (u, v, poids)."""
    aretes = []
    n = len(SITES_ACPM)
    for i in range(n):
        for j in range(i + 1, n):
            aretes.append((SITES_ACPM[i], SITES_ACPM[j], MATRICE_ACPM[i][j]))
    return aretes


# ---------------------------------------------------------------------------
# 3) TACHES DE PRODUCTION QUOTIDIENNE -- pour PERT
#    Chaque tache : code, libelle, duree (min), liste des predecesseurs.
# ---------------------------------------------------------------------------
TACHES = [
    {"code": "P1", "libelle": "Pesee ingredients",   "duree": 20, "predecesseurs": []},
    {"code": "P2", "libelle": "Cuisson",             "duree": 40, "predecesseurs": ["P1"]},
    {"code": "P3", "libelle": "Assaisonnement",      "duree": 15, "predecesseurs": ["P2"]},
    {"code": "P4", "libelle": "Conditionnement",     "duree": 30, "predecesseurs": ["P3"]},
    {"code": "L1", "libelle": "Chargement camion",   "duree": 10, "predecesseurs": ["P4"]},
    {"code": "L2", "libelle": "Livraison client A",  "duree": 12, "predecesseurs": ["L1"]},
    {"code": "L3", "libelle": "Livraison client B",  "duree": 18, "predecesseurs": ["L1"]},
    {"code": "L4", "libelle": "Retour labo",         "duree": 25, "predecesseurs": ["L2", "L3"]},
]

# Le projet debute a 8h00 (t = 0)
HEURE_DEBUT_MIN = 8 * 60  # 480 minutes depuis minuit
DEADLINE_MIN = 9 * 60 + 30  # 9h30 = 570 minutes depuis minuit


# ---------------------------------------------------------------------------
# 4) PROGRAMMATION LINEAIRE (optimisation des quantites produites)
#    Variables : S (standard), V (vegetarien), G (sans gluten)
# ---------------------------------------------------------------------------
TYPES_REPAS = ["S", "V", "G"]

# Marge beneficiaire unitaire (en euros) : on maximise 3S + 4V + 5G
MARGES = {"S": 3, "V": 4, "G": 5}

# Contraintes de ressources : nom -> (coef_S, coef_V, coef_G, disponibilite)
RESSOURCES = {
    "Legumes (kg)":        (0.5, 1.0, 0.3, 100),
    "Viande (kg)":         (0.8, 0.0, 0.0,  80),
    "Cereales (kg)":       (0.4, 0.6, 0.9, 120),
    "Temps cuisson (min)": (2.0, 1.5, 3.0, 480),
    "Main d'oeuvre (min)": (1.5, 1.2, 2.0, 600),
}

# Contraintes commerciales : demande maximale par type
DEMANDE_MAX = {"S": 100, "V": 80, "G": 50}
# Minimum impose pour raisons marketing
PRODUCTION_MIN = {"S": 20, "V": 20, "G": 20}
# Nombre total de repas
TOTAL_MAX = 200
