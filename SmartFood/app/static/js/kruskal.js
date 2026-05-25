// Visualisation interactive de l'arbre couvrant de poids minimal (Kruskal).
(function () {
    const data = JSON.parse(document.getElementById("data-kruskal").textContent);

    // Ensemble des aretes de l'ACPM (clef "u|v" non orientee)
    const cleArbre = new Set();
    data.arbre.forEach(function (a) {
        cleArbre.add(a.from + "|" + a.to);
        cleArbre.add(a.to + "|" + a.from);
    });

    function couleurSite(id) {
        if (id === "E") return "#1565c0";
        if (/^C[1-4]$/.test(id)) return "#f57c00"; // cuisines C1..C4
        return "#66bb6a";                          // clients A,B,C,D,F
    }

    const noeuds = new vis.DataSet(
        data.sites.map(function (s) {
            return {
                id: s,
                label: s,
                color: { background: couleurSite(s), border: "#37474f" },
                font: { color: "#fff", size: 16 },
                shape: "circle",
            };
        })
    );

    // Toutes les aretes : ACPM en vert epais, les autres en gris tres clair
    const aretes = new vis.DataSet(
        data.aretes_completes.map(function (a, i) {
            const dansArbre = cleArbre.has(a.from + "|" + a.to);
            return {
                id: i,
                from: a.from,
                to: a.to,
                label: dansArbre ? String(a.poids) : "",
                color: { color: dansArbre ? "#2e7d32" : "#eceff1" },
                width: dansArbre ? 4 : 1,
                font: { size: 13, color: "#2e7d32" },
                hidden: false,
                smooth: { type: "continuous" },
            };
        })
    );

    const container = document.getElementById("graphe-kruskal");
    new vis.Network(container, { nodes: noeuds, edges: aretes }, {
        layout: { improvedLayout: true },
        physics: { stabilization: true, barnesHut: { springLength: 160, avoidOverlap: 0.2 } },
        interaction: { hover: true },
    });
})();
