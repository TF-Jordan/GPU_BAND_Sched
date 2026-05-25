// Visualisations PERT : diagramme de Gantt + reseau potentiel-taches.
(function () {
    const data = JSON.parse(document.getElementById("data-pert").textContent);

    // Base temporelle : 8h00 du jour courant. Les dates PERT (en minutes) y sont ajoutees.
    const base = new Date();
    base.setHours(8, 0, 0, 0);
    function aDate(minutes) {
        return new Date(base.getTime() + minutes * 60000);
    }

    // ---------------------------------------------------------------
    // 1) Diagramme de Gantt (vis-timeline)
    // ---------------------------------------------------------------
    const items = new vis.DataSet(
        data.taches.map(function (t, i) {
            return {
                id: i,
                content: t.code + " &middot; " + t.libelle,
                start: aDate(t.tot_debut),
                end: aDate(t.tot_fin),
                title: t.libelle + " (" + t.duree + " min)" +
                       " | marge totale : " + t.marge_totale + " min",
                className: t.critique ? "gantt-critique" : "gantt-normale",
            };
        })
    );

    const ganttContainer = document.getElementById("gantt");
    new vis.Timeline(ganttContainer, items, {
        stack: true,
        showCurrentTime: false,
        min: aDate(-10),
        max: aDate(data.duree_projet + 15),
        editable: false,
        margin: { item: 8 },
        orientation: "top",
        format: {
            minorLabels: { minute: "HH:mm", hour: "HH:mm" },
            majorLabels: { hour: "HH:mm" },
        },
    });

    // ---------------------------------------------------------------
    // 2) Reseau potentiel-taches (vis-network)
    // ---------------------------------------------------------------
    const critiques = new Set(data.chemin_critique);

    const noeuds = new vis.DataSet(
        data.taches.map(function (t) {
            return {
                id: t.code,
                label: t.code + "\n[" + t.tot_debut + " | " + t.tard_debut + "]",
                title: t.libelle + " (" + t.duree + " min)",
                color: {
                    background: t.critique ? "#d32f2f" : "#42a5f5",
                    border: "#37474f",
                },
                font: { color: "#fff", multi: true, size: 14 },
                shape: "box",
                margin: 10,
            };
        })
    );

    // Arcs de precedence
    const arcs = [];
    let arcId = 0;
    data.taches.forEach(function (t) {
        t.predecesseurs.forEach(function (p) {
            const arcCritique = critiques.has(p) && critiques.has(t.code);
            arcs.push({
                id: arcId++,
                from: p,
                to: t.code,
                arrows: "to",
                color: { color: arcCritique ? "#d32f2f" : "#b0bec5" },
                width: arcCritique ? 3 : 1,
            });
        });
    });
    const aretes = new vis.DataSet(arcs);

    const reseauContainer = document.getElementById("reseau-pert");
    new vis.Network(reseauContainer, { nodes: noeuds, edges: aretes }, {
        layout: {
            hierarchical: {
                direction: "LR",
                sortMethod: "directed",
                levelSeparation: 140,
                nodeSpacing: 90,
            },
        },
        physics: false,
        interaction: { hover: true },
    });
})();
