"""Module 4 -- Optimisation des quantites produites (programmation lineaire).

Formulation du programme lineaire (sujet, section 3) :

  Variables de decision (continues, >= 0) :
    S = nombre de repas standard
    V = nombre de repas vegetariens
    G = nombre de repas sans gluten

  Fonction objectif (a MAXIMISER) -- marge totale quotidienne (euros) :
    max Z = 3 S + 4 V + 5 G

  Contraintes de ressources :
    0.5 S + 1.0 V + 0.3 G <= 100   (legumes, kg)
    0.8 S                 <=  80   (viande, kg)
    0.4 S + 0.6 V + 0.9 G <= 120   (cereales, kg)
    2.0 S + 1.5 V + 3.0 G <= 480   (temps de cuisson, min)
    1.5 S + 1.2 V + 2.0 G <= 600   (main d'oeuvre, min)

  Contraintes commerciales :
    S <= 100, V <= 80, G <= 50     (demande maximale)
    S >= 20,  V >= 20, G >= 20     (minimum marketing)
    S + V + G <= 200               (capacite totale)

Resolution avec scipy.optimize.linprog (qui MINIMISE : on minimise donc -Z).
"""

from scipy.optimize import linprog

from app.data import (
    TYPES_REPAS,
    MARGES,
    RESSOURCES,
    DEMANDE_MAX,
    PRODUCTION_MIN,
    TOTAL_MAX,
)


def solve():
    """Resout le programme lineaire et retourne un dictionnaire JSON."""
    # Ordre des variables : [S, V, G]
    # On maximise 3S + 4V + 5G  <=>  on minimise -(3S + 4V + 5G)
    c = [-MARGES["S"], -MARGES["V"], -MARGES["G"]]

    # Contraintes d'inegalite A_ub x <= b_ub
    A_ub = []
    b_ub = []
    libelles_contraintes = []

    # 1) Ressources
    for nom, (cs, cv, cg, dispo) in RESSOURCES.items():
        A_ub.append([cs, cv, cg])
        b_ub.append(dispo)
        libelles_contraintes.append(nom)

    # 2) Capacite totale : S + V + G <= 200
    A_ub.append([1, 1, 1])
    b_ub.append(TOTAL_MAX)
    libelles_contraintes.append("Total repas")

    # Bornes (min marketing <= variable <= demande max)
    bounds = [
        (PRODUCTION_MIN["S"], DEMANDE_MAX["S"]),
        (PRODUCTION_MIN["V"], DEMANDE_MAX["V"]),
        (PRODUCTION_MIN["G"], DEMANDE_MAX["G"]),
    ]

    res = linprog(c=c, A_ub=A_ub, b_ub=b_ub, bounds=bounds, method="highs")

    if not res.success:
        return {"success": False, "message": res.message}

    s_opt, v_opt, g_opt = (float(x) for x in res.x)
    marge_totale = float(-res.fun)  # on avait minimise -Z

    solution = {
        "S": round(s_opt, 4),
        "V": round(v_opt, 4),
        "G": round(g_opt, 4),
    }

    # Analyse de l'utilisation des ressources (contrainte saturee = active)
    utilisation = []
    for nom, (cs, cv, cg, dispo) in RESSOURCES.items():
        consomme = cs * s_opt + cv * v_opt + cg * g_opt
        sature = abs(consomme - dispo) < 1e-6
        utilisation.append({
            "ressource": nom,
            "consomme": round(consomme, 2),
            "disponible": dispo,
            "reste": round(dispo - consomme, 2),
            "taux": round(100 * consomme / dispo, 1) if dispo else 0.0,
            "sature": sature,
        })

    total_repas = s_opt + v_opt + g_opt

    return {
        "success": True,
        "objectif": "Maximiser Z = 3 S + 4 V + 5 G",
        "solution": solution,
        "marge_totale": round(marge_totale, 2),
        "total_repas": round(total_repas, 2),
        "total_max": TOTAL_MAX,
        "marges_unitaires": MARGES,
        "demande_max": DEMANDE_MAX,
        "production_min": PRODUCTION_MIN,
        "utilisation_ressources": utilisation,
        # Formulation lisible (pour affichage)
        "formulation": _formulation_texte(),
    }


def _formulation_texte():
    """Renvoie la formulation mathematique sous forme de lignes affichables."""
    contraintes = []
    for nom, (cs, cv, cg, dispo) in RESSOURCES.items():
        termes = []
        for coef, var in ((cs, "S"), (cv, "V"), (cg, "G")):
            if coef != 0:
                termes.append(f"{coef:g} {var}")
        contraintes.append({
            "expression": " + ".join(termes) + f" <= {dispo}",
            "nom": nom,
        })
    contraintes.append({"expression": "S + V + G <= 200", "nom": "Total repas"})
    return {
        "objectif": "max Z = 3 S + 4 V + 5 G",
        "contraintes_ressources": contraintes,
        "bornes": [
            "20 <= S <= 100",
            "20 <= V <= 80",
            "20 <= G <= 50",
        ],
    }


if __name__ == "__main__":
    import json
    print(json.dumps(solve(), indent=2, ensure_ascii=False))
