"""Application FastAPI -- SmartFood : optimisation de la livraison et production.

Presente sur une interface web les resultats des quatre modules :
  1. Plus courts chemins (Dijkstra)
  2. Arbre couvrant de poids minimal (Kruskal / ACPM)
  3. Ordonnancement de la production (PERT)
  4. Optimisation des quantites produites (programmation lineaire)

Lancement :  uvicorn app.main:app --reload
             (depuis le dossier SmartFood/)
"""

from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.responses import HTMLResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates

from app.modules import dijkstra, kruskal, pert, linear_prog

BASE_DIR = Path(__file__).resolve().parent

app = FastAPI(
    title="SmartFood",
    description="Optimisation de la livraison et de la production de repas",
    version="1.0.0",
)

app.mount("/static", StaticFiles(directory=BASE_DIR / "static"), name="static")
templates = Jinja2Templates(directory=BASE_DIR / "templates")


def _num(v):
    """Filtre Jinja2 : formate un nombre en supprimant les zeros superflus."""
    if v is None:
        return "—"
    if isinstance(v, bool):
        return str(v)
    try:
        f = float(v)
    except (TypeError, ValueError):
        return str(v)
    if abs(f) < 1e-9:
        return "0"
    if f == int(f) and abs(f) < 1e15:
        return str(int(f))
    return f"{f:g}"


templates.env.filters["num"] = _num


# ---------------------------------------------------------------------------
# Pages (rendu Jinja2)
# ---------------------------------------------------------------------------
@app.get("/", response_class=HTMLResponse)
def page_accueil(request: Request):
    return templates.TemplateResponse(request, "index.html", {
        "titre": "Accueil",
        "dijkstra": dijkstra.solve(),
        "kruskal": kruskal.solve(),
        "pert": pert.solve(),
        "pl": linear_prog.solve(),
    })


@app.get("/dijkstra", response_class=HTMLResponse)
def page_dijkstra(request: Request):
    return templates.TemplateResponse(request, "dijkstra.html", {
        "titre": "Plus courts chemins (Dijkstra)",
        "data": dijkstra.solve(),
    })


@app.get("/kruskal", response_class=HTMLResponse)
def page_kruskal(request: Request):
    return templates.TemplateResponse(request, "kruskal.html", {
        "titre": "ACPM - Cablage fibre (Kruskal)",
        "data": kruskal.solve(),
    })


@app.get("/pert", response_class=HTMLResponse)
def page_pert(request: Request):
    return templates.TemplateResponse(request, "pert.html", {
        "titre": "Ordonnancement de la production (PERT)",
        "data": pert.solve(),
    })


@app.get("/programmation-lineaire", response_class=HTMLResponse)
def page_pl(request: Request):
    return templates.TemplateResponse(request, "linear_prog.html", {
        "titre": "Optimisation des quantites (Programmation lineaire)",
        "data": linear_prog.solve(),
    })


# ---------------------------------------------------------------------------
# API JSON (consommee par les visualisations interactives + docs /docs)
# ---------------------------------------------------------------------------
@app.get("/api/dijkstra")
def api_dijkstra():
    return JSONResponse(dijkstra.solve())


@app.get("/api/kruskal")
def api_kruskal():
    return JSONResponse(kruskal.solve())


@app.get("/api/pert")
def api_pert():
    return JSONResponse(pert.solve())


@app.get("/api/programmation-lineaire")
def api_pl():
    return JSONResponse(linear_prog.solve())
