"""Module 4 -- Optimisation des quantites produites (programmation lineaire).

Formulation :
  max Z = 3S + 4V + 5G
  s.c. contraintes de ressources, bornes commerciales (20 <= x <= demande_max)

Resolution manuelle par la methode du simplexe en tableau :
  - Changement de variable S'=S-20, V'=V-20, G'=G-20 pour obtenir x' >= 0
  - 9 contraintes (5 ressources + total + 3 bornes superieures) avec slacks e1..e9
  - Chaque iteration est enregistree (tableau complet) pour affichage pedagogique
"""

from app.data import MARGES, RESSOURCES, DEMANDE_MAX, PRODUCTION_MIN, TOTAL_MAX


def _sn(x):
    """Nettoie un flottant pour le stockage : arrondi 4 dec., -0 => 0, entier si possible."""
    if x is None:
        return None
    v = round(float(x), 4)
    if abs(v) < 1e-9:
        return 0
    if abs(v - round(v)) < 1e-9:
        return int(round(v))
    return v


def _simplex():
    """Methode du simplexe avec enregistrement complet de chaque tableau.

    Changement de variable : S' = S-20,  V' = V-20,  G' = G-20
      => Z = 3S'+4V'+5G' + 240   (CONST = 240)

    9 contraintes (slacks e1..e9) :
      (C1) 0.5S'+V'+0.3G'      + e1                        = 64   (legumes)
      (C2) 0.8S'               + e2                        = 64   (viande)
      (C3) 0.4S'+0.6V'+0.9G'   + e3                        = 82   (cereales)
      (C4) 2S'+1.5V'+3G'        + e4                        = 350  (cuisson)
      (C5) 1.5S'+1.2V'+2G'      + e5                        = 506  (main d'oeuvre)
      (C6) S'+V'+G'             + e6                        = 140  (total repas)
      (C7) S'                   + e7                        = 80   (S'<=80)
      (C8) V'                   + e8                        = 60   (V'<=60)
      (C9) G'                   + e9                        = 30   (G'<=30)

    Base initiale : {e1,...,e9}  =>  solution de base initiale S'=V'=G'=0, Z'=0
    """
    NOMS = ["S'", "V'", "G'", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9"]
    N, M = 12, 9
    CONST = 240

    # Coefficients de la fonction objectif (maximisation)
    c = [3.0, 4.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]

    # Matrice A et second membre b (apres substitution)
    A = [
        [0.5, 1.0, 0.3, 1, 0, 0, 0, 0, 0, 0, 0, 0],  # C1 legumes   b=64
        [0.8, 0.0, 0.0, 0, 1, 0, 0, 0, 0, 0, 0, 0],  # C2 viande    b=64
        [0.4, 0.6, 0.9, 0, 0, 1, 0, 0, 0, 0, 0, 0],  # C3 cereales  b=82
        [2.0, 1.5, 3.0, 0, 0, 0, 1, 0, 0, 0, 0, 0],  # C4 cuisson   b=350
        [1.5, 1.2, 2.0, 0, 0, 0, 0, 1, 0, 0, 0, 0],  # C5 MO        b=506
        [1.0, 1.0, 1.0, 0, 0, 0, 0, 0, 1, 0, 0, 0],  # C6 total     b=140
        [1.0, 0.0, 0.0, 0, 0, 0, 0, 0, 0, 1, 0, 0],  # C7 S'<=80    b=80
        [0.0, 1.0, 0.0, 0, 0, 0, 0, 0, 0, 0, 1, 0],  # C8 V'<=60    b=60
        [0.0, 0.0, 1.0, 0, 0, 0, 0, 0, 0, 0, 0, 1],  # C9 G'<=30    b=30
    ]
    b = [64.0, 64.0, 82.0, 350.0, 506.0, 140.0, 80.0, 60.0, 30.0]

    LIBELLES = [
        "Legumes : 0.5S'+V'+0.3G' = 64",
        "Viande : 0.8S' = 64",
        "Cereales : 0.4S'+0.6V'+0.9G' = 82",
        "Cuisson : 2S'+1.5V'+3G' = 350",
        "Main-d'oeuvre : 1.5S'+1.2V'+2G' = 506",
        "Total repas : S'+V'+G' = 140",
        "Borne S' <= 80",
        "Borne V' <= 60",
        "Borne G' <= 30",
    ]

    base = list(range(3, 12))  # indices de e1..e9

    # Tableau des contraintes en forme standard (pour affichage)
    contraintes_std = [
        {
            "num": f"C{i+1}",
            "libelle": LIBELLES[i],
            "ecart": NOMS[3 + i],
            "b": int(b[i]) if b[i] == int(b[i]) else b[i],
        }
        for i in range(M)
    ]

    iterations = []

    for it_num in range(20):
        c_base = [c[base[i]] for i in range(M)]
        z_j = [sum(c_base[i] * A[i][j] for i in range(M)) for j in range(N)]
        d_j = [c[j] - z_j[j] for j in range(N)]
        Z_val = sum(c_base[i] * b[i] for i in range(M))

        snap = {
            "numero": it_num,
            "base": [NOMS[base[i]] for i in range(M)],
            "c_j": [_sn(c[j]) for j in range(N)],
            "lignes": [
                {
                    "var": NOMS[base[i]],
                    "c_b": _sn(c[base[i]]),
                    "coeffs": [_sn(A[i][j]) for j in range(N)],
                    "b": _sn(b[i]),
                }
                for i in range(M)
            ],
            "z_j": [_sn(x) for x in z_j],
            "d_j": [_sn(x) for x in d_j],
            "z_val": _sn(Z_val),
            "j_ent": -1,
            "i_sort": -1,
            "ratios": [None] * M,
            "var_entrante": None,
            "var_sortante": None,
            "pivot": None,
        }

        max_dj = max(d_j)

        if max_dj <= 1e-9:
            snap["optimal"] = True
            snap["commentaire"] = (
                f"Tous les dⱼ = cⱼ − zⱼ sont ≤ 0 : "
                f"la solution est optimale. "
                f"Z’ = {_sn(Z_val)},  Z = Z’ + {CONST} = "
                f"{_sn(Z_val + CONST)} €."
            )
            iterations.append(snap)
            break

        # Variable entrante : max d_j
        j_ent = d_j.index(max_dj)

        # Rapports min (test du pivot)
        rapports = []
        for i in range(M):
            if A[i][j_ent] > 1e-9:
                r = b[i] / A[i][j_ent]
                snap["ratios"][i] = _sn(r)
                rapports.append((r, i))

        if not rapports:
            snap["optimal"] = False
            snap["commentaire"] = "Probleme non borne."
            iterations.append(snap)
            break

        _, i_sort = min(rapports)

        snap["optimal"] = False
        snap["j_ent"] = j_ent
        snap["i_sort"] = i_sort
        snap["var_entrante"] = NOMS[j_ent]
        snap["var_sortante"] = NOMS[base[i_sort]]
        snap["pivot"] = _sn(A[i_sort][j_ent])
        snap["commentaire"] = (
            f"Le dⱼ le plus positif est d({NOMS[j_ent]}) = {_sn(max_dj)} "
            f"➡ {NOMS[j_ent]} entre dans la base. "
            f"Le rapport minimum est {_sn(snap['ratios'][i_sort])} "
            f"(ligne {NOMS[base[i_sort]]}) "
            f"➡ {NOMS[base[i_sort]]} sort de la base. "
            f"Element pivot : {_sn(A[i_sort][j_ent])}."
        )
        iterations.append(snap)

        # --- Operation de pivot ---
        piv = A[i_sort][j_ent]
        A[i_sort] = [x / piv for x in A[i_sort]]
        b[i_sort] /= piv
        for i in range(M):
            if i != i_sort:
                fac = A[i][j_ent]
                if abs(fac) > 1e-12:
                    A[i] = [A[i][k] - fac * A[i_sort][k] for k in range(N)]
                    b[i] -= fac * b[i_sort]
        base[i_sort] = j_ent

    sol_p = {NOMS[base[i]]: b[i] for i in range(M)}

    return {
        "noms_cols": NOMS,
        "constante_shift": CONST,
        "contraintes_std": contraintes_std,
        "iterations": iterations,
        "solution_shiftee": {
            "S'": _sn(sol_p.get("S'", 0)),
            "V'": _sn(sol_p.get("V'", 0)),
            "G'": _sn(sol_p.get("G'", 0)),
        },
    }


def solve():
    """Resout le PL par le simplexe manuel et retourne le dictionnaire JSON complet."""
    simplex = _simplex()

    sol_p = simplex["solution_shiftee"]
    S_opt = sol_p["S'"] + 20
    V_opt = sol_p["V'"] + 20
    G_opt = sol_p["G'"] + 20
    Z_opt = MARGES["S"] * S_opt + MARGES["V"] * V_opt + MARGES["G"] * G_opt

    solution = {"S": _sn(S_opt), "V": _sn(V_opt), "G": _sn(G_opt)}

    utilisation = []
    for nom, (cs, cv, cg, dispo) in RESSOURCES.items():
        consomme = cs * S_opt + cv * V_opt + cg * G_opt
        sature = abs(consomme - dispo) < 1e-6
        utilisation.append({
            "ressource": nom,
            "consomme": round(consomme, 2),
            "disponible": dispo,
            "reste": round(dispo - consomme, 2),
            "taux": round(100 * consomme / dispo, 1) if dispo else 0.0,
            "sature": sature,
        })

    return {
        "success": True,
        "objectif": "Maximiser Z = 3 S + 4 V + 5 G",
        "solution": solution,
        "marge_totale": round(Z_opt, 2),
        "total_repas": round(S_opt + V_opt + G_opt, 2),
        "total_max": TOTAL_MAX,
        "marges_unitaires": MARGES,
        "demande_max": DEMANDE_MAX,
        "production_min": PRODUCTION_MIN,
        "utilisation_ressources": utilisation,
        "formulation": _formulation_texte(),
        "simplex": simplex,
    }


def _formulation_texte():
    contraintes = []
    for nom, (cs, cv, cg, dispo) in RESSOURCES.items():
        termes = []
        for coef, var in ((cs, "S"), (cv, "V"), (cg, "G")):
            if coef != 0:
                termes.append(f"{coef:g} {var}")
        contraintes.append({
            "expression": " + ".join(termes) + f" ≤ {dispo}",
            "nom": nom,
        })
    contraintes.append({"expression": "S + V + G ≤ 200", "nom": "Total repas"})
    return {
        "objectif": "max Z = 3 S + 4 V + 5 G",
        "contraintes_ressources": contraintes,
        "bornes": ["20 ≤ S ≤ 100", "20 ≤ V ≤ 80", "20 ≤ G ≤ 50"],
    }


if __name__ == "__main__":
    import json
    print(json.dumps(solve(), indent=2, ensure_ascii=False))
