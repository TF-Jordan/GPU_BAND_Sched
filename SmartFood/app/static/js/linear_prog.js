// Visualisations de la programmation lineaire (Chart.js).
(function () {
    const data = JSON.parse(document.getElementById("data-pl").textContent);

    // ---------------------------------------------------------------
    // 1) Quantites produites (S, V, G) vs demande maximale
    // ---------------------------------------------------------------
    const ctxQ = document.getElementById("chart-quantites");
    new Chart(ctxQ, {
        type: "bar",
        data: {
            labels: ["Standard (S)", "Vegetarien (V)", "Sans gluten (G)"],
            datasets: [
                {
                    label: "Quantite optimale",
                    data: [data.solution.S, data.solution.V, data.solution.G],
                    backgroundColor: ["#42a5f5", "#66bb6a", "#f57c00"],
                },
                {
                    label: "Demande maximale",
                    data: [data.demande_max.S, data.demande_max.V, data.demande_max.G],
                    backgroundColor: "rgba(96,125,139,.2)",
                },
            ],
        },
        options: {
            responsive: true,
            maintainAspectRatio: false,
            plugins: { legend: { position: "bottom" } },
            scales: { y: { beginAtZero: true, title: { display: true, text: "Repas" } } },
        },
    });

    // ---------------------------------------------------------------
    // 2) Taux d'utilisation des ressources (%)
    // ---------------------------------------------------------------
    const ctxR = document.getElementById("chart-ressources");
    const labels = data.utilisation_ressources.map(function (u) { return u.ressource; });
    const taux = data.utilisation_ressources.map(function (u) { return u.taux; });
    const couleurs = data.utilisation_ressources.map(function (u) {
        return u.sature ? "#d32f2f" : "#66bb6a";
    });

    new Chart(ctxR, {
        type: "bar",
        data: {
            labels: labels,
            datasets: [{
                label: "Taux d'utilisation (%)",
                data: taux,
                backgroundColor: couleurs,
            }],
        },
        options: {
            indexAxis: "y",
            responsive: true,
            maintainAspectRatio: false,
            plugins: {
                legend: { display: false },
                tooltip: {
                    callbacks: {
                        afterLabel: function (ctx) {
                            const u = data.utilisation_ressources[ctx.dataIndex];
                            return u.consomme + " / " + u.disponible +
                                   (u.sature ? "  (SATUREE)" : "");
                        },
                    },
                },
            },
            scales: {
                x: { beginAtZero: true, max: 100, title: { display: true, text: "%" } },
            },
        },
    });
})();
