# Protocole réseau — omega-remote-paging V1

## Format de trame

Toutes les trames (requête et réponse) partagent le même format d'en-tête.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|          magic (0x524D)       |   opcode      |     flags     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           vm_id (u32)                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
|                          page_id (u64)                        |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        payload_len (u32)                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
|                    payload (payload_len octets)               |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

**Taille d'en-tête : 20 octets (fixe)**
**Byte order : big-endian pour tous les champs multi-octets**

### Champs

| Offset | Taille | Champ       | Description                                                 |
|--------|--------|-------------|-------------------------------------------------------------|
| 0      | 2      | magic       | `0x524D` ("RM" — Remote Memory) — identifiant du protocole |
| 2      | 1      | opcode      | Type de message (voir tableau des opcodes)                  |
| 3      | 1      | flags       | Bit 0 : page compressée (V2) — `0x00` en V1               |
| 4      | 4      | vm_id       | Identifiant de la VM (u32 big-endian)                       |
| 8      | 8      | page_id     | Identifiant de la page dans la VM (u64 big-endian)          |
| 16     | 4      | payload_len | Taille du payload en octets (u32 big-endian)                |
| 20     | *      | payload     | Données (page ou message d'erreur ou JSON)                  |

### Contraintes

- Taille maximale du payload : `2 × PAGE_SIZE = 8192` octets (protection DoS)
- Taille de page : `PAGE_SIZE = 4096` octets
- Pour `PUT_PAGE` : payload_len doit être exactement 4096

## Tableau des opcodes

### Requêtes (client → store)

| Opcode | Valeur | Payload req. | Description                              |
|--------|--------|--------------|------------------------------------------|
| PING   | 0x01   | —            | Vérification de connectivité             |
| PUT_PAGE | 0x10 | 4096 octets  | Stocke une page (vm_id, page_id) → data  |
| GET_PAGE | 0x11 | —            | Récupère la page (vm_id, page_id)        |
| DELETE_PAGE | 0x12 | —         | Supprime la page (vm_id, page_id)        |
| STATS_REQUEST | 0x20 | —      | Demande les statistiques du store        |

### Réponses (store → client)

| Opcode | Valeur | Payload rép. | Description                              |
|--------|--------|--------------|------------------------------------------|
| PONG   | 0x02   | —            | Réponse à PING                           |
| OK     | 0x80   | variable     | Opération réussie (+ données si GET)     |
| NOT_FOUND | 0x81 | —           | Page absente du store                    |
| ERROR  | 0x82   | message UTF8 | Erreur (message texte dans payload)      |
| STATS_RESPONSE | 0x21 | JSON UTF8 | Statistiques du store               |

## Séquences d'échange

### PING / PONG

```
Client                           Store
  │                                │
  │──── PING (vm_id=0, page_id=0) ─►│
  │                                │
  │◄─── PONG ──────────────────────│
  │                                │
```

### PUT_PAGE

```
Client                           Store
  │                                │
  │──── PUT_PAGE ──────────────────►│
  │     vm_id=42, page_id=1234     │
  │     payload=<4096 octets>      │
  │                                │
  │◄─── OK (vm_id=42, page_id=1234)│  [page stockée]
  │                                │
  │──── PUT_PAGE (store plein) ────►│
  │                                │
  │◄─── ERROR ("store plein...") ──│  [si max_pages atteint]
  │                                │
```

### GET_PAGE

```
Client                           Store
  │                                │
  │──── GET_PAGE ──────────────────►│
  │     vm_id=42, page_id=1234     │
  │                                │
  │◄─── OK ────────────────────────│  [page trouvée]
  │     payload=<4096 octets>      │
  │                                │
  │──── GET_PAGE ──────────────────►│
  │     vm_id=42, page_id=9999     │  [page inconnue]
  │                                │
  │◄─── NOT_FOUND ─────────────────│
  │                                │
```

### STATS_REQUEST / STATS_RESPONSE

```
Client                           Store
  │                                │
  │──── STATS_REQUEST ─────────────►│
  │                                │
  │◄─── STATS_RESPONSE ────────────│
  │     payload={"pages_stored":5, │
  │       "estimated_bytes":20480, │
  │       "node_id":"node-b"}      │
  │                                │
```

## Gestion des erreurs réseau

### Côté client (node-a-agent)

- Timeout configurable (défaut : 2000ms) sur connexion, lecture et écriture
- En cas d'erreur : connexion marquée invalide, reconnexion automatique au prochain appel
- Comportement de secours pour les page faults : injection d'une page zéro si le store est injoignable (la VM continue, données potentiellement incorrectes — loggué en WARNING)

### Côté serveur (node-bc-store)

- Connexion reset / EOF → loggué en DEBUG (comportement normal)
- Erreurs protocole (magic incorrect, opcode inconnu, payload trop grand) → réponse ERROR + fermeture connexion

## État d'implémentation

| Fonctionnalité        | État                                               |
|-----------------------|----------------------------------------------------|
| Protocole binaire TCP | Implémenté (V1) — stable                          |
| TLS sur port 9100     | Implémenté (V4) — certificats auto-signés, TOFU   |
| Compression pages     | Flag 0x01 réservé — non activé par défaut          |
| Réplication           | Factor configurable (OMEGA_REPLICATION_FACTOR)     |
| Préfetch              | Non implémenté                                     |
| RDMA                  | Non implémenté — prévu si latence TCP insuffisante |

## Canal de contrôle HTTP (V4)

En V4, le canal de contrôle n'utilise pas de nouveaux opcodes TCP. Il passe par
une API HTTP séparée sur le port 9300 (`/control/*`). Les opcodes TCP sont réservés
au transfert de pages (store protocol).
