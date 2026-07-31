# PRD — hum: Local Development TUI

**Nome prodotto:** `hum`
**Tagline:** Keep your local stack humming.
**Tipo:** CLI e TUI per orchestrazione di processi locali di sviluppo
**Stack previsto:** Rust, Ratatui, Tokio
**Stato:** Draft
**Versione:** 0.1
**Target:** team di sviluppo Compri (primo progetto configurato con `hum`)

---

## 1. Sintesi

`hum` è uno strumento da terminale per avviare, arrestare, monitorare e diagnosticare i processi necessari allo sviluppo locale di un prodotto composto da più servizi.

Il primo utilizzo previsto è il progetto **Compri**, il cui team lavora su più repository e servizi, prevalentemente applicazioni Node.js, frontend React, API, worker e altri processi locali. Attualmente ogni sviluppatore deve ricordare:

* quali repository servono per una determinata attività;
* dove si trovano localmente;
* quali comandi eseguire;
* in quale ordine avviare i processi;
* quali porte devono essere disponibili;
* quali servizi sono effettivamente funzionanti;
* dove leggere i log;
* quali variabili d’ambiente o dipendenze mancano.

`hum` centralizza queste informazioni in una configurazione dichiarativa condivisa e offre una TUI avviabile con:

```bash
hum
```

Lo strumento non vuole sostituire Docker Compose, Kubernetes, tmux o un process manager da produzione. È un orchestratore leggero e specifico per lo sviluppo locale, pensato per essere applicabile a qualsiasi progetto multi-servizio — a partire da Compri.

---

## 2. Problema

Lo sviluppo locale richiede l’esecuzione coordinata di processi distribuiti tra più repository.

Un flusso tipico (esempio: il progetto Compri) può richiedere:

```text
compri-applications
└── procurement-frontend

compri-api
└── api-server

compri-agents
└── agent-worker

compri-integrations
└── integrations-worker
```

Ogni servizio può avere:

* una directory di lavoro differente;
* un comando di avvio differente;
* una porta specifica;
* dipendenze da altri servizi;
* variabili d’ambiente obbligatorie;
* un proprio health check;
* procedure differenti per installazione e bootstrap.

Questo produce diversi problemi.

### 2.1 Complessità operativa

Gli sviluppatori devono aprire più terminali e avviare manualmente ogni processo.

### 2.2 Conoscenza non condivisa

Le istruzioni sono distribuite tra README, messaggi Slack, documentazione interna e conoscenza individuale.

### 2.3 Stato ambiguo

Non è immediato capire se un servizio:

* non è stato avviato;
* è in fase di avvio;
* è attivo ma non risponde;
* è bloccato da una dipendenza;
* è terminato con errore;
* sta usando una porta differente da quella attesa.

### 2.4 Configurazioni diverse tra sviluppatori

I repository possono trovarsi in percorsi differenti e alcuni sviluppatori possono utilizzare porte, comandi o file `.env` personalizzati.

### 2.5 Debug lento

Quando l’ambiente locale non funziona, una parte significativa del tempo viene spesa per identificare problemi infrastrutturali anziché lavorare sulla feature.

---

## 3. Visione

Uno sviluppatore deve poter entrare nel proprio ambiente locale con un solo comando:

```bash
hum
```

La TUI deve rispondere immediatamente a quattro domande:

1. Quali servizi sono disponibili?
2. Quali servizi sono attivi?
3. Quali servizi devo avviare per il lavoro che sto facendo?
4. Perché un servizio non funziona?

Quando tutti i servizi girano correttamente, il sistema "hums" — funziona in modo regolare e silenzioso, senza bisogno di intervento.

---

## 4. Obiettivi

### 4.1 Obiettivi principali

* Avviare e arrestare più processi locali da un’unica interfaccia.
* Dichiarare servizi e profili attraverso un file YAML.
* Mostrare lo stato dei processi in tempo reale.
* Centralizzare i log senza richiedere terminali separati.
* Supportare repository e working directory differenti.
* Verificare porte e health check HTTP o TCP.
* Gestire dipendenze semplici tra servizi.
* Diagnosticare problemi comuni dell’ambiente locale.
* Ridurre il tempo necessario per iniziare a sviluppare.
* Rendere la configurazione dell’ambiente condivisibile nel team.

### 4.2 Obiettivi secondari

* Mostrare branch e stato Git dei repository.
* Aprire nel browser gli URL associati ai servizi.
* Supportare override locali non versionati.
* Rendere più semplice l’onboarding di nuovi sviluppatori.
* Fornire una CLI utilizzabile anche senza TUI.

---

## 5. Non obiettivi

La prima versione di `hum` non deve:

* sostituire Docker Compose;
* gestire deployment o processi di produzione;
* orchestrare container;
* gestire cluster Kubernetes;
* eseguire processi su host remoti;
* includere una REST API;
* includere autenticazione o autorizzazione;
* supportare repliche di uno stesso servizio;
* includere un sistema di scheduling o cron;
* includere un plugin system;
* gestire segreti aziendali;
* sostituire un package manager;
* mantenere i processi attivi dopo la chiusura del processo principale;
* funzionare come daemon in background.

Queste funzionalità potranno essere valutate separatamente soltanto in presenza di una necessità concreta.

---

## 6. Utenti

### 6.1 Sviluppatore frontend

Vuole avviare il frontend e il minimo numero di servizi backend necessari.

Esempio:

```bash
hum up frontend
```

### 6.2 Sviluppatore backend

Vuole avviare API, worker e dipendenze richieste senza avviare tutte le applicazioni frontend.

```bash
hum up backend
```

### 6.3 Sviluppatore full-stack

Vuole avviare l’intero ambiente locale.

```bash
hum up full
```

### 6.4 Nuovo membro del team

Vuole sapere quali repository installare, quali file `.env` creare e quali dipendenze mancano.

```bash
hum doctor
```

---

## 7. Concetti principali

### 7.1 Service

Un processo avviabile e monitorabile da `hum`.

Esempi:

* frontend React;
* API Node.js;
* worker;
* Storybook;
* consumer di una coda;
* database avviato tramite comando esterno;
* mock server.

### 7.2 Profile

Un insieme nominato di servizi necessario per uno specifico contesto di sviluppo.

Esempi:

* `frontend`;
* `backend`;
* `agents`;
* `ui`;
* `full`.

### 7.3 Dependency

Una relazione secondo cui un servizio richiede che un altro servizio venga avviato prima.

### 7.4 Health check

Una verifica che determina se il servizio è realmente disponibile.

Tipi inizialmente supportati:

* HTTP;
* TCP;
* processo attivo;
* porta in ascolto.

### 7.5 Doctor check

Una verifica statica o dinamica dell’ambiente locale, per esempio:

* repository mancante;
* directory inesistente;
* comando non disponibile;
* dipendenze non installate;
* file `.env` mancante;
* variabile d’ambiente mancante;
* porta occupata;
* branch Git differente da quello previsto.

---

## 8. User experience

### 8.1 Avvio

```bash
hum
```

Se la configurazione è valida, viene aperta la TUI.

```text
┌ hum ────────────────────────────────────────────────────────────┐
│ Profile: frontend                         Environment: local    │
├──────────────────────┬────────────┬───────┬─────────────────────┤
│ Service              │ Status     │ Port  │ Health              │
├──────────────────────┼────────────┼───────┼─────────────────────┤
│ frontend             │ ● running  │ 5173  │ healthy             │
│ api                  │ ● running  │ 3000  │ healthy             │
│ agents               │ ○ stopped  │ 3001  │ —                   │
│ integrations-worker  │ ! blocked  │ —     │ api unavailable     │
└──────────────────────┴────────────┴───────┴─────────────────────┘

[space] start/stop  [r] restart  [enter] details  [l] logs
[p] profiles       [d] doctor   [o] open URL     [q] quit
```

### 8.2 Selezione del profilo

Premendo `p`, l’utente può scegliere un profilo.

```text
Select profile

> frontend
  backend
  agents
  ui
  full
```

Dopo la selezione, la TUI evidenzia i servizi inclusi nel profilo e permette di avviarli.

### 8.3 Vista log

```text
┌ api — logs ─────────────────────────────────────────────────────┐
│ 10:42:01  Starting development server                          │
│ 10:42:02  Connected to database                                │
│ 10:42:02  Listening on http://localhost:3000                    │
│ 10:42:11  GET /health 200                                      │
└─────────────────────────────────────────────────────────────────┘

[f] follow  [c] clear  [/] search  [esc] back
```

### 8.4 Dettagli del servizio

```text
api

Status        Healthy
PID           41382
Uptime        00:32:18
Repository    ~/code/compri/compri-api
Branch        feature/new-evaluation
Command       pnpm dev
Port          3000
Health check  GET http://localhost:3000/health
Last result   200 OK, 42 ms
```

### 8.5 Diagnostica

```text
api

✓ Repository found
✓ pnpm available
✓ node available
✓ Dependencies installed
✓ .env.local found
✗ Missing environment variable: AUTH_SECRET
✗ Port 3000 is already used by PID 18423
```

---

## 9. CLI

La TUI deve essere l’esperienza principale, ma le operazioni fondamentali devono essere disponibili anche come comandi non interattivi.

### 9.1 Comandi MVP

```bash
hum
hum up <profile>
hum up <service...>
hum down
hum stop <service...>
hum restart <service>
hum status
hum logs <service>
hum doctor
hum config validate
```

### 9.2 Esempi

```bash
hum up frontend
hum up api agents
hum restart api
hum logs integrations-worker
hum doctor
```

### 9.3 Exit code

I comandi non interattivi devono restituire exit code coerenti:

```text
0  Operazione completata
1  Errore generico
2  Configurazione non valida
3  Servizio non trovato
4  Avvio del servizio fallito
5  Health check fallito
6  Diagnostica non superata
```

---

## 10. Configurazione

### 10.1 Formato

La prima versione utilizza YAML.

File condiviso:

```text
hum.yaml
```

Override locale:

```text
hum.local.yaml
```

`hum.local.yaml` deve normalmente essere escluso da Git.

La configurazione locale può sovrascrivere:

* percorsi dei repository;
* porte;
* variabili d’ambiente;
* file `.env`;
* comandi;
* URL;
* comportamento della TUI.

### 10.2 Esempio

Esempio di configurazione per il progetto Compri:

```yaml
version: 1

repositories:
  applications:
    path: ~/code/compri/compri-applications

  api:
    path: ~/code/compri/compri-api

  agents:
    path: ~/code/compri/compri-agents

services:
  frontend:
    repository: applications
    cwd: apps/procurement-frontend
    command: pnpm dev
    port: 5173
    url: http://localhost:5173

    healthcheck:
      type: http
      url: http://localhost:5173
      interval: 2s
      timeout: 1s
      retries: 15

    requires:
      commands:
        - node
        - pnpm
      files:
        - .env.local
      env:
        - VITE_API_URL

    depends_on:
      - api

  api:
    repository: api
    command: pnpm dev
    port: 3000
    url: http://localhost:3000

    healthcheck:
      type: http
      url: http://localhost:3000/health
      interval: 2s
      timeout: 1s
      retries: 20

    requires:
      commands:
        - node
        - pnpm
      files:
        - .env
      env:
        - DATABASE_URL
        - AUTH_SECRET

  agents:
    repository: agents
    command: pnpm dev
    port: 3001

    healthcheck:
      type: tcp
      host: localhost
      port: 3001

    depends_on:
      - api

  storybook:
    repository: applications
    cwd: packages/ui
    command: pnpm storybook
    port: 6006
    url: http://localhost:6006

profiles:
  frontend:
    services:
      - frontend
      - api

  backend:
    services:
      - api
      - agents

  ui:
    services:
      - storybook

  full:
    services:
      - frontend
      - api
      - agents
      - storybook
```

### 10.3 Override locale

```yaml
repositories:
  applications:
    path: ~/Projects/compri-applications

services:
  frontend:
    port: 5174
    env_file: .env.federico
```

### 10.4 Risoluzione della configurazione

Ordine di precedenza:

```text
valori predefiniti
        ↓
hum.yaml
        ↓
hum.local.yaml
        ↓
variabili d’ambiente
        ↓
argomenti CLI
```

---

## 11. Requisiti funzionali

### RF-01 — Parsing della configurazione

Il sistema deve leggere e validare `hum.yaml`.

In caso di errore deve mostrare:

* file;
* campo;
* posizione, quando disponibile;
* descrizione comprensibile;
* possibile correzione.

### RF-02 — Discovery della configurazione

`hum` deve cercare il file di configurazione:

1. nel percorso passato tramite `--config`;
2. nella directory corrente;
3. risalendo le directory parent;
4. in un percorso globale opzionale.

### RF-03 — Avvio di un servizio

Il sistema deve avviare il comando configurato nella relativa working directory.

### RF-04 — Avvio di un profilo

Il sistema deve risolvere tutti i servizi del profilo e le rispettive dipendenze.

### RF-05 — Ordinamento delle dipendenze

Le dipendenze devono essere risolte tramite grafo aciclico diretto.

Il sistema deve rilevare e rifiutare dipendenze circolari.

### RF-06 — Attesa delle dipendenze

Un servizio può essere avviato:

* quando il processo dipendente è stato avviato;
* oppure quando la dipendenza è healthy.

Il comportamento deve essere configurabile. Nell’MVP il default è `healthy`, quando esiste un health check.

### RF-07 — Arresto di un servizio

Il sistema deve inviare un segnale di terminazione al processo e ai suoi processi discendenti.

Ordine previsto:

1. richiesta di terminazione graceful;
2. attesa configurabile;
3. terminazione forzata.

### RF-08 — Arresto globale

Quando l’utente chiude `hum`, il sistema deve chiedere:

```text
Stop all running services before quitting? [Y/n]
```

Nell’MVP i processi sono posseduti dalla sessione `hum` e non devono rimanere attivi accidentalmente.

### RF-09 — Restart

Il sistema deve poter arrestare e riavviare un singolo servizio senza influire sugli altri.

### RF-10 — Stato del processo

Il sistema deve distinguere almeno:

```text
Stopped
Starting
Running
Healthy
Unhealthy
Stopping
Failed
Blocked
```

### RF-11 — Cattura dei log

Il sistema deve catturare separatamente:

* `stdout`;
* `stderr`.

Ogni riga deve contenere:

* timestamp;
* nome del servizio;
* stream di origine;
* contenuto.

### RF-12 — Buffer dei log

I log devono essere conservati in memoria tramite un buffer circolare con dimensione configurabile.

Il valore predefinito può essere di 10.000 righe per servizio.

### RF-13 — Health check HTTP

Il sistema deve supportare richieste HTTP con:

* URL;
* timeout;
* intervallo;
* numero di tentativi;
* status code attesi.

### RF-14 — Health check TCP

Il sistema deve supportare la verifica di connessione a una porta TCP.

### RF-15 — Port check

Prima dell’avvio, il sistema deve verificare se la porta configurata è già occupata.

La TUI deve mostrare, quando disponibile:

* PID;
* nome del processo;
* comando.

### RF-16 — Doctor

`hum doctor` deve controllare almeno:

* esistenza dei repository;
* esistenza delle working directory;
* disponibilità dei comandi richiesti;
* esistenza dei file richiesti;
* presenza delle variabili d’ambiente richieste;
* disponibilità delle porte;
* validità della configurazione;
* dipendenze circolari;
* presenza di `node_modules`, quando configurato.

### RF-17 — Git metadata

Per ogni repository il sistema deve poter mostrare:

* branch corrente;
* commit abbreviato;
* working tree dirty o clean.

Questa funzionalità non deve impedire l’avvio del servizio.

### RF-18 — Apertura URL

Premendo `o`, il sistema deve aprire nel browser l’URL configurato per il servizio selezionato.

### RF-19 — Refresh

La TUI deve aggiornare automaticamente:

* stato dei processi;
* uptime;
* health check;
* log;
* porta;
* eventuali errori.

### RF-20 — Comandi non interattivi

Le operazioni fondamentali devono poter essere eseguite senza inizializzare l’interfaccia Ratatui.

---

## 12. Requisiti non funzionali

### RNF-01 — Distribuzione

`hum` deve essere distribuito come singolo binario.

### RNF-02 — Avvio rapido

La TUI deve essere visibile entro circa 200 ms in presenza di una configurazione locale valida, esclusi i controlli di rete e health check.

### RNF-03 — Consumo di risorse

Con dieci servizi attivi, `hum` non deve introdurre un consumo significativo rispetto ai processi gestiti.

### RNF-04 — Stabilità

Il crash di un servizio figlio non deve causare il crash della TUI.

### RNF-05 — Cross-platform

Priorità iniziale:

1. macOS;
2. Linux.

Windows non è un requisito dell’MVP.

### RNF-06 — Errori leggibili

Gli errori devono spiegare:

* cosa è successo;
* quale servizio è coinvolto;
* quale comando è fallito;
* quale possibile azione può risolvere il problema.

### RNF-07 — Configurazione versionabile

La configurazione condivisa non deve contenere percorsi assoluti specifici di un singolo sviluppatore.

### RNF-08 — Nessun lock-in

Il file di configurazione deve rimanere semplice e leggibile anche senza utilizzare `hum`.

---

## 13. Architettura proposta

```text
┌─────────────────────────────────────────┐
│                 hum-cli                 │
│ CLI arguments, output non interattivo   │
└───────────────────┬─────────────────────┘
                    │
┌───────────────────▼─────────────────────┐
│                 hum-tui                 │
│ Ratatui, input, views, rendering        │
└───────────────────┬─────────────────────┘
                    │
┌───────────────────▼─────────────────────┐
│                hum-core                 │
│ State machine, profiles, dependencies   │
└───────┬───────────────────────┬─────────┘
        │                       │
┌───────▼──────────┐   ┌────────▼─────────┐
│  hum-runtime     │   │  hum-config      │
│ processes, logs, │   │ YAML, validation │
│ signals, health  │   │ overrides        │
└──────────────────┘   └──────────────────┘
```

Possibile workspace:

```text
hum/
├── Cargo.toml
├── crates/
│   ├── hum-cli/
│   ├── hum-tui/
│   ├── hum-core/
│   ├── hum-config/
│   └── hum-runtime/
├── examples/
│   └── hum.example.yaml
└── docs/
    └── PRD.md
```

Per l’MVP può essere utilizzato anche un unico crate con moduli interni, evitando una separazione prematura.

---

## 14. Modello dati indicativo

```rust
struct Config {
    version: u32,
    repositories: HashMap<String, RepositoryConfig>,
    services: HashMap<String, ServiceConfig>,
    profiles: HashMap<String, ProfileConfig>,
}

struct RepositoryConfig {
    path: PathBuf,
}

struct ServiceConfig {
    repository: Option<String>,
    cwd: Option<PathBuf>,
    command: String,
    port: Option<u16>,
    url: Option<String>,
    env_file: Option<PathBuf>,
    env: HashMap<String, String>,
    depends_on: Vec<String>,
    healthcheck: Option<HealthcheckConfig>,
    requires: RequirementsConfig,
}

struct ProfileConfig {
    services: Vec<String>,
}

enum HealthcheckConfig {
    Http {
        url: String,
        timeout: Duration,
        interval: Duration,
        retries: u32,
    },
    Tcp {
        host: String,
        port: u16,
        timeout: Duration,
        interval: Duration,
        retries: u32,
    },
}

enum ServiceStatus {
    Stopped,
    Starting,
    Running,
    Healthy,
    Unhealthy,
    Stopping,
    Failed,
    Blocked,
}
```

---

## 15. Gestione dei processi

### 15.1 Ownership

Nell’MVP tutti i servizi avviati sono processi figli di `hum`.

```text
hum
├── frontend
├── api
└── agents
```

Non viene utilizzato un daemon.

### 15.2 Process group

Ogni servizio deve essere avviato in un process group dedicato, così da poter terminare anche eventuali processi discendenti generati dal comando Node.

Questo è necessario per casi come:

```text
pnpm
└── vite
    └── esbuild
```

### 15.3 Shell

La prima versione può eseguire i comandi attraverso la shell dell’utente.

Esempio:

```yaml
command: pnpm dev
```

Deve essere documentato che i comandi sono considerati fidati perché provengono dalla configurazione del progetto.

### 15.4 Crash

Quando un processo termina inaspettatamente:

* lo stato diventa `Failed`;
* viene mostrato l’exit code;
* i log rimangono disponibili;
* il processo non viene riavviato automaticamente nell’MVP;
* l’utente può premere `r` per riavviarlo.

---

## 16. Stato e health

Lo stato del processo e la salute del servizio devono rimanere distinti.

Esempio:

```text
Process: running
Port: listening
Health: unhealthy
```

Un processo attivo non implica che l’applicazione sia pronta.

Transizione indicativa:

```text
Stopped
   ↓ start
Starting
   ↓ process alive
Running
   ↓ health check succeeds
Healthy
   ↓ health check fails
Unhealthy
   ↓ process exits
Failed
```

Un servizio è `Blocked` quando una dipendenza richiesta non può essere avviata o non diventa healthy.

---

## 17. Navigazione TUI

### 17.1 Layout principale

La schermata principale contiene:

* header;
* profilo selezionato;
* tabella servizi;
* eventuale pannello log;
* status bar con shortcut.

### 17.2 Shortcut MVP

```text
↑ / k       Servizio precedente
↓ / j       Servizio successivo
space       Start o stop
r           Restart
enter       Dettagli
l           Log
p           Seleziona profilo
d           Doctor
o           Apri URL
?           Help
q           Esci
```

### 17.3 Modalità log

```text
f           Abilita/disabilita follow
c           Pulisci buffer visualizzato
/           Cerca
esc         Torna alla lista
```

---

## 18. Sicurezza

`hum` esegue comandi locali definiti nella configurazione.

Per questo motivo:

* la configurazione deve essere considerata codice fidato;
* non devono essere scaricate ed eseguite configurazioni remote automaticamente;
* i valori delle variabili d’ambiente sensibili non devono essere mostrati;
* i log devono poter mascherare pattern configurati;
* `hum doctor` deve mostrare il nome delle variabili mancanti, non il loro contenuto;
* eventuali file `.env` non devono essere copiati o sincronizzati dallo strumento.

---

## 19. MVP

La prima release utilizzabile deve includere:

1. Parsing e validazione YAML.
2. Configurazione di repository, servizi e profili.
3. Avvio e arresto dei processi.
4. Gestione dei process group.
5. Risoluzione delle dipendenze.
6. TUI con lista dei servizi.
7. Stato dei processi.
8. Cattura e visualizzazione dei log.
9. Health check HTTP.
10. Health check TCP.
11. Controllo delle porte.
12. Comando `doctor`.
13. Comandi `up`, `down`, `status`, `logs` e `restart`.
14. Override tramite `hum.local.yaml`.
15. Shutdown controllato con `Ctrl+C`.

---

## 20. Funzionalità successive

### Versione 0.2

* ricerca nei log;
* filtro dei log per livello;
* supporto a variabili interpolate;
* setup guidato dei percorsi locali;
* bootstrap dei repository;
* comandi `install` o `setup`;
* gruppi visuali;
* restart automatico opzionale;
* notifiche desktop in caso di crash;
* esportazione diagnostica.

### Versione 0.3

* modalità detached;
* processo daemon locale;
* comando `hum attach`;
* persistenza dello stato;
* più sessioni o workspace;
* reload della configurazione.

La modalità daemon deve essere introdotta solo dopo averne verificato la reale necessità.

---

## 21. Metriche di successo

Il progetto è considerato efficace quando:

* un nuovo sviluppatore può avviare un ambiente funzionante senza consultare più README;
* un profilo di sviluppo può essere avviato con un singolo comando;
* lo stato dei servizi è comprensibile in meno di dieci secondi;
* i conflitti di porta vengono identificati automaticamente;
* le variabili d’ambiente mancanti vengono segnalate prima dell’avvio;
* diminuisce il numero di messaggi interni del tipo “quali servizi devo avviare?”;
* diminuisce il tempo medio necessario per diagnosticare un ambiente locale non funzionante.

---

## 22. Criteri di accettazione MVP

### Configurazione

* Una configurazione valida viene caricata correttamente.
* Una configurazione non valida produce un errore leggibile.
* Gli override locali sovrascrivono i valori condivisi.
* Le dipendenze circolari vengono rilevate.

### Processi

* Un servizio può essere avviato dalla TUI.
* Un servizio può essere arrestato dalla TUI.
* Un servizio può essere riavviato.
* L’intero process group viene terminato.
* Il crash di un servizio non chiude `hum`.

### Profili

* Un profilo avvia tutti i servizi configurati.
* Le dipendenze vengono incluse automaticamente.
* I servizi vengono avviati nell’ordine corretto.
* Un servizio bloccato mostra la causa.

### Health

* Un health check HTTP modifica lo stato del servizio.
* Un health check TCP modifica lo stato del servizio.
* Un processo running ma unhealthy viene rappresentato correttamente.

### Log

* `stdout` e `stderr` vengono catturati.
* I log sono consultabili durante e dopo l’esecuzione.
* Il buffer non cresce senza limiti.
* I log di un servizio non bloccano gli altri processi.

### Doctor

* I repository mancanti vengono segnalati.
* I comandi mancanti vengono segnalati.
* I file `.env` mancanti vengono segnalati.
* Le variabili richieste mancanti vengono segnalate.
* Le porte occupate vengono segnalate.

### Shutdown

* `Ctrl+C` produce uno shutdown controllato.
* L’utente può scegliere se terminare tutti i servizi.
* Non rimangono processi figli involontariamente attivi.

---

## 23. Rischi

### Complessità della gestione dei processi

La gestione corretta dei segnali e dei processi discendenti è più complessa dell’avvio di un semplice `Command`.

**Mitigazione:** limitare inizialmente le piattaforme a macOS e Linux e utilizzare process group dedicati.

### Scope creep

Il progetto potrebbe evolvere rapidamente verso un clone di Docker Compose o Process Compose.

**Mitigazione:** ogni nuova funzionalità deve rispondere a un problema osservato in un workflow reale (a partire da quello di Compri).

### Configurazione troppo complessa

Aggiungere template, condizioni, interpolazioni e scripting potrebbe trasformare YAML in un linguaggio di programmazione.

**Mitigazione:** mantenere lo schema dichiarativo e preferire convenzioni semplici.

### Dipendenza eccessiva dalla TUI

La TUI potrebbe rendere più difficile l’utilizzo in script o CI.

**Mitigazione:** mantenere un core indipendente dalla UI e fornire comandi CLI non interattivi.

### Differenze tra macOS e Linux

Process inspection, segnali e apertura del browser possono comportarsi diversamente.

**Mitigazione:** isolare le operazioni platform-specific nel runtime.

---

## 24. Principi di prodotto

1. **Local-first**
   Tutto avviene sulla macchina dello sviluppatore.

2. **Configuration-driven**
   Il comportamento è descritto da un file versionabile.

3. **Observable**
   Stato, log, porte e health devono essere sempre visibili.

4. **Explain failures**
   Non basta mostrare che qualcosa non funziona: bisogna spiegare perché.

5. **Small scope**
   `hum` deve risolvere bene il workflow di sviluppo locale multi-servizio, non diventare un orchestratore universale.

6. **Terminal-native**
   La UX deve essere progettata per tastiera e terminale, non imitare una dashboard web.

7. **Composable**
   Lo strumento deve utilizzare i normali comandi dei repository senza imporre una nuova struttura applicativa.

8. **Project-agnostic**
   `hum` non è legato a un solo progetto: Compri è il primo caso d’uso, non un vincolo architetturale.

---

## 25. Decisioni iniziali

| Area                | Decisione                     |
| ------------------- | ------------------------------|
| Nome prodotto        | `hum`                          |
| Linguaggio          | Rust                          |
| Framework TUI       | Ratatui                       |
| Runtime asincrono   | Tokio                         |
| Configurazione      | YAML                          |
| File condiviso      | `hum.yaml`                    |
| Override locale     | `hum.local.yaml`              |
| Process ownership   | Processi figli della sessione |
| Daemon              | Escluso dall’MVP              |
| Piattaforme         | macOS e Linux                 |
| Health check        | HTTP e TCP                    |
| Restart automatico  | Escluso dall’MVP              |
| Persistenza log     | Solo memoria nell’MVP         |
| Config remote       | Non supportate                |
| CLI non interattiva | Supportata                    |
| Primo progetto configurato | Compri                |

---

## 26. Domande aperte

* La configurazione di Compri per `hum` deve vivere in un repository dedicato oppure nel monorepo principale?
* Il profilo selezionato deve essere ricordato tra sessioni?
* `hum up frontend` deve aprire la TUI oppure rimanere non interattivo?
* Lo shutdown del processo principale deve terminare sempre i servizi o chiedere conferma?
* È necessario supportare processi Docker tramite normali comandi shell?
* I check su `node_modules` devono essere convenzionali o configurabili?
* È necessario supportare più file `.env` per servizio?
* I repository devono poter dichiarare un proprio frammento di configurazione?
* Il team ha necessità reale di processi detached?

---

## 27. Release iniziale proposta

La prima release interna può essere considerata pronta quando uno sviluppatore del team Compri può eseguire:

```bash
git clone <repository-configurazione-compri>
cd <repository-configurazione-compri>
hum doctor
hum up frontend
```

e ottenere:

* verifica dell’ambiente;
* avvio automatico dei servizi richiesti;
* visualizzazione dello stato;
* accesso centralizzato ai log;
* shutdown controllato.

La priorità non è supportare ogni possibile processo, ma rendere affidabile e immediato il workflow di sviluppo locale più comune del team Compri, dimostrando che `hum` è applicabile anche ad altri progetti in futuro.
