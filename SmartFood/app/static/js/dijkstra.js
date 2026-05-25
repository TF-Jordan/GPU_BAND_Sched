// Visualisation interactive du graphe oriente et des plus courts chemins (Dijkstra).
(function () {
    const data = JSON.parse(document.getElementById("data-dijkstra").textContent);

    // Couleur d'un sommet selon son role
    function couleurSommet(id) {
        if (id === data.entrepot) return "#1565c0";      // entrepot (bleu)
        if (id === data.labo) return "#607d8b";           // labo (gris)
        if (data.cuisines.includes(id)) return "#f57c00"; // cuisine (orange)
        if (data.clients.includes(id)) return "#66bb6a";  // client (vert)
        return "#90a4ae";
    }

    // Construction des noeuds
    const noeuds = new vis.DataSet(
        data.sommets.map(function (s) {
            const dist = data.distances[s];
            return {
                id: s,
                label: s + (dist !== null ? "\n(" + dist + ")" : "\n(∞)"),
                color: { background: couleurSommet(s), border: "#37474f" },
                font: { color: "#fff", multi: true, size: 16 },
                shape: "circle",
            };
        })
    );

    // Construction des arcs (orientes)
    const aretes = new vis.DataSet(
        data.arcs.map(function (a, i) {
            return {
                id: i,
                from: a.from,
                to: a.to,
                label: String(a.poids),
                arrows: "to",
                color: { color: "#b0bec5" },
                font: { align: "top", size: 12 },
                smooth: { type: "curvedCW", roundness: 0.15 },
            };
        })
    );

    const container = document.getElementById("graphe-dijkstra");
    const network = new vis.Network(container, { nodes: noeuds, edges: aretes }, {
        layout: { improvedLayout: true },
        physics: { stabilization: true, barnesHut: { springLength: 140 } },
        interaction: { hover: true },
    });

    // Met en evidence le trajet d'un client
    function surligner(client) {
        const trajet = data.trajets.find(function (t) { return t.client === client; });

        // Reinitialiser
        aretes.forEach(function (e) {
            aretes.update({ id: e.id, color: { color: "#b0bec5" }, width: 1 });
        });
        noeuds.forEach(function (n) {
            noeuds.update({ id: n.id, borderWidth: 1 });
        });

        if (!trajet || !trajet.atteignable) return;

        const chemin = trajet.chemin_complet;
        // Surligner les arcs du chemin
        for (let i = 0; i < chemin.length - 1; i++) {
            const u = chemin[i], v = chemin[i + 1];
            aretes.forEach(function (e) {
                if (e.from === u && e.to === v) {
                    aretes.update({ id: e.id, color: { color: "#2e7d32" }, width: 4 });
                }
            });
        }
        // Surligner les sommets du chemin
        chemin.forEach(function (s) {
            noeuds.update({ id: s, borderWidth: 4 });
        });
    }

    // Boutons clients
    const conteneurBoutons = document.getElementById("boutons-clients");
    data.clients.forEach(function (client) {
        const trajet = data.trajets.find(function (t) { return t.client === client; });
        const btn = document.createElement("button");
        btn.className = "btn-client";
        btn.textContent = "Client " + client;
        if (!trajet || !trajet.atteignable) {
            btn.disabled = true;
            btn.title = "Non atteignable";
        }
        btn.addEventListener("click", function () {
            document.querySelectorAll(".btn-client").forEach(function (b) {
                b.classList.remove("actif");
            });
            btn.classList.add("actif");
            surligner(client);
        });
        conteneurBoutons.appendChild(btn);
    });

    // Surligner le meilleur trajet par defaut
    network.once("stabilizationIterationsDone", function () {
        const best = data.comparaison.find(function (t) { return t.atteignable; });
        if (best) {
            surligner(best.client);
            const boutons = document.querySelectorAll(".btn-client");
            boutons.forEach(function (b) {
                if (b.textContent === "Client " + best.client) b.classList.add("actif");
            });
        }
    });
})();
