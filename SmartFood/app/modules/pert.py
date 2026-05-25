"""Module 3 -- Ordonnancement de la production (methode PERT / potentiel-taches).

Implementation conforme aux cours d'ordonnancement de l'enseignante
(technique d'ordonnancement.pdf et les diapositives PERT) :

  - Date au plus tot  : T_j = Max( T_i + duree(i, j) ) pour tous les predecesseurs i de j
                        (initialisation : taches sans predecesseur -> date au plus tot = 0)
  - Date au plus tard : T*_i = Min( T*_j - duree(i, j) ) pour tous les successeurs j de i
                        (initialisation : dernier sommet T*_fin = T_fin)
  - Marge totale  i = (date au plus tard de debut) - (date au plus tot de debut)
  - Marge libre   i = Min( date au plus tot des successeurs ) - (date au plus tot de fin)
  - Chemin critique : taches dont la marge totale est nulle
    (les dates au plus tot et au plus tard y coincident).

On utilise la representation "potentiel-taches" (chaque tache est un sommet),
exactement comme le tableau du sujet (Tache | Duree | Predecesseurs).
"""

from app.data import TACHES, HEURE_DEBUT_MIN, DEADLINE_MIN


def _index_taches():
    """Retourne (dict code->tache, dict code->successeurs)."""
    taches = {t["code"]: dict(t) for t in TACHES}
    successeurs = {code: [] for code in taches}
    for t in TACHES:
        for pred in t["predecesseurs"]:
            successeurs[pred].append(t["code"])
    return taches, successeurs


def _ordre_topologique(taches, successeurs):
    """Tri topologique (methode des niveaux du cours) : taches sans antecedent
    d'abord, puis celles dont tous les antecedents sont deja places, etc."""
    degre_entrant = {code: len(t["predecesseurs"]) for code, t in taches.items()}
    file = [code for code, d in degre_entrant.items() if d == 0]
    ordre = []
    while file:
        file.sort()  # ordre stable et reproductible
        courant = file.pop(0)
        ordre.append(courant)
        for succ in successeurs[courant]:
            degre_entrant[succ] -= 1
            if degre_entrant[succ] == 0:
                file.append(succ)
    return ordre


def calculer_pert():
    """Calcule dates au plus tot/tard, marges et chemin critique.

    Retourne un dictionnaire avec, pour chaque tache : duree, predecesseurs,
    date au plus tot (debut/fin), date au plus tard (debut/fin), marge totale,
    marge libre, et appartenance au chemin critique.
    """
    taches, successeurs = _index_taches()
    ordre = _ordre_topologique(taches, successeurs)

    # --- Passe avant : dates au plus tot ---
    tot_debut = {}  # date au plus tot de DEBUT de la tache
    tot_fin = {}    # date au plus tot de FIN de la tache
    for code in ordre:
        preds = taches[code]["predecesseurs"]
        if not preds:
            tot_debut[code] = 0
        else:
            tot_debut[code] = max(tot_fin[p] for p in preds)
        tot_fin[code] = tot_debut[code] + taches[code]["duree"]

    duree_projet = max(tot_fin.values())

    # --- Passe arriere : dates au plus tard ---
    tard_fin = {}    # date au plus tard de FIN de la tache
    tard_debut = {}  # date au plus tard de DEBUT de la tache
    for code in reversed(ordre):
        succs = successeurs[code]
        if not succs:
            tard_fin[code] = duree_projet
        else:
            tard_fin[code] = min(tard_debut[s] for s in succs)
        tard_debut[code] = tard_fin[code] - taches[code]["duree"]

    # --- Marges ---
    resultats = []
    for code in ordre:
        marge_totale = tard_debut[code] - tot_debut[code]
        succs = successeurs[code]
        if not succs:
            marge_libre = duree_projet - tot_fin[code]
        else:
            marge_libre = min(tot_debut[s] for s in succs) - tot_fin[code]
        critique = (marge_totale == 0)
        resultats.append({
            "code": code,
            "libelle": taches[code]["libelle"],
            "duree": taches[code]["duree"],
            "predecesseurs": taches[code]["predecesseurs"],
            "successeurs": succs,
            "tot_debut": tot_debut[code],
            "tot_fin": tot_fin[code],
            "tard_debut": tard_debut[code],
            "tard_fin": tard_fin[code],
            "marge_totale": marge_totale,
            "marge_libre": marge_libre,
            "critique": critique,
        })

    # --- Chemin critique (sequence ordonnee de taches critiques) ---
    chemin_critique = _extraire_chemin_critique(
        taches, successeurs, tot_debut, tard_debut
    )

    return {
        "ordre_topologique": ordre,
        "taches": resultats,
        "duree_projet": duree_projet,
        "chemin_critique": chemin_critique,
    }


def _extraire_chemin_critique(taches, successeurs, tot_debut, tard_debut):
    """Construit la sequence du chemin critique en suivant les taches de marge nulle."""
    critiques = {c for c in taches if tard_debut[c] - tot_debut[c] == 0}
    # Depart : tache critique sans predecesseur critique
    debut = None
    for code in sorted(critiques):
        if not any(p in critiques for p in taches[code]["predecesseurs"]):
            debut = code
            break
    if debut is None:
        return sorted(critiques)

    chemin = [debut]
    courant = debut
    while True:
        succ_critiques = [s for s in successeurs[courant] if s in critiques]
        if not succ_critiques:
            break
        # En cas de plusieurs, suivre celui qui prolonge la chaine (le plus tot)
        courant = min(succ_critiques, key=lambda s: tot_debut[s])
        chemin.append(courant)
    return chemin


def _format_heure(minutes):
    """Convertit un nombre de minutes depuis minuit en 'HhMM'."""
    total = HEURE_DEBUT_MIN + minutes
    h = total // 60
    m = total % 60
    return f"{h:02d}h{m:02d}"


def analyser_deadline():
    """Analyse l'impact d'un deadline impose a 9h30 (objectif 4.b du sujet)."""
    pert = calculer_pert()
    duree_projet = pert["duree_projet"]
    fin_reelle_min = HEURE_DEBUT_MIN + duree_projet  # minutes depuis minuit
    budget_min = DEADLINE_MIN - HEURE_DEBUT_MIN  # temps disponible avant deadline
    reduction_necessaire = max(0, duree_projet - budget_min)

    return {
        "duree_projet": duree_projet,
        "heure_fin_actuelle": _format_heure(duree_projet),
        "deadline": "09h30",
        "budget_minutes": budget_min,
        "respecte": duree_projet <= budget_min,
        "reduction_necessaire": reduction_necessaire,
        "taches_critiques": pert["chemin_critique"],
        "commentaire": (
            f"Le projet se termine a {_format_heure(duree_projet)} "
            f"({duree_projet} min apres 8h00). "
            + (
                f"Le deadline de 9h30 ({budget_min} min) n'est PAS respecte : "
                f"il faut reduire la duree de {reduction_necessaire} min, "
                f"obligatoirement sur une tache du chemin critique."
                if duree_projet > budget_min else
                f"Le deadline de 9h30 est respecte."
            )
        ),
    }


def analyser_second_livreur():
    """Analyse l'impact d'ajouter un second livreur (analyse de sensibilite)."""
    return {
        "constat": (
            "Les livraisons L2 (client A, 12 min) et L3 (client B, 18 min) "
            "succedent toutes deux a L1 et sont independantes : sans contrainte "
            "de ressources, elles s'executent deja en parallele."
        ),
        "impact": (
            "Ajouter un second livreur ne reduit donc PAS la duree du projet : "
            "le goulot d'etranglement est la chaine de production "
            "P1->P2->P3->P4->L1 puis L3->L4, et non la disponibilite des livreurs. "
            "Le chemin critique reste inchange."
        ),
    }


def solve():
    """Resultat complet du module PERT, serialisable en JSON."""
    pert = calculer_pert()
    # Ajout des heures reelles (8h00 + t) pour le Gantt
    for t in pert["taches"]:
        t["debut_heure"] = _format_heure(t["tot_debut"])
        t["fin_heure"] = _format_heure(t["tot_fin"])
    pert["deadline"] = analyser_deadline()
    pert["second_livreur"] = analyser_second_livreur()
    pert["heure_debut"] = "08h00"
    return pert


if __name__ == "__main__":
    import json
    print(json.dumps(solve(), indent=2, ensure_ascii=False))
