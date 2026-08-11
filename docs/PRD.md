# PRD — hum: local development process launcher and monitor

**Nome prodotto:** `hum`

**Tagline:** Keep your local stack humming.

**Tipo:** CLI e TUI per orchestrare runtime locali di sviluppo

**Stack:** Rust, Ratatui, Tokio

**Stato:** Draft

**Versione prodotto:** 0.3

**Versione configurazione progetto:** 3

> La v3 estende senza invalidare i file v1/v2 process-only. Le integrazioni di
> prodotto appartengono a pack di configurazione esterni al repository `hum`.

## 1. Sintesi

`hum` avvia, arresta, monitora e diagnostica servizi locali attraverso adapter
generici. Il grafo può includere processi, servizi Docker Compose e task
one-shot dichiarativi.

Il flusso principale è esplicito:

```bash
hum <project> <template> <command>
```

Per esempio:

```bash
hum demo all-services start
```

`start` avvia i servizi selezionati tramite il rispettivo runtime e poi termina.
Non rimane un daemon `hum` residente. Una successiva invocazione della CLI o
della TUI ritrova i servizi tramite un runtime registry persistente e verifica
il loro stato interrogando il sistema operativo.

La TUI è un osservatore e controller del runtime, non il proprietario del
lifetime dei servizi. Aprirla o chiuderla non avvia né arresta implicitamente
i processi.

## 2. Problema

Lo sviluppo locale di un prodotto multi-servizio richiede di ricordare:

- repository e working directory;
- comandi e variabili d'ambiente;
- servizi necessari per una specifica attività;
- ordine delle dipendenze;
- porte e health check;
- PID, stato e log dei processi avviati;
- procedure di arresto e recupero dopo un crash.

Un orchestratore che conserva tutto solo nella memoria della sessione non può
supportare comandi separati come `start`, `status`, `logs` e `stop`. Inoltre
costringe la TUI a rimanere aperta e lega i servizi alle sue pipe di output.

`hum` risolve il problema mantenendo dichiarativa la configurazione e
persistendo soltanto i metadati runtime minimi. I processi applicativi restano
normali processi locali, ispezionabili anche senza `hum`.

## 3. Concetti

### 3.1 Project

Un prodotto locale registrato con un nome stabile, per esempio `demo`. Il
registry globale associa il nome al file di configurazione del progetto.

### 3.2 Runtime

Un adapter nominato e configurato dal progetto. La v3 include `process` e
`compose`; nessun adapter contiene nomi, immagini o policy di prodotto.

### 3.3 Service

Un processo avviabile e monitorabile: frontend, API, worker, Storybook, mock
server o un database avviato tramite un comando locale.

L'identità runtime è `<project>/<service>`. Lo stesso servizio incluso in due
template non viene avviato due volte.

### 3.4 Task

Unità one-shot trusted-code eseguita come argv diretto, con timeout, dipendenze
e controllo opzionale di idempotenza. Non è un servizio persistente.

### 3.5 Environment provider

Provider nominato che risolve sorgenti dotenv per il solo child interessato.
La prima implementazione usa 1Password CLI senza esportare valori nel processo
globale di `hum`.

### 3.6 Template

Un insieme nominato di servizi per un contesto di lavoro, per esempio
`frontend`, `backend` o `all-services`. Sostituisce il concetto v1 di `profile`.

Un template seleziona servizi; non possiede i processi. Fermare un template
significa richiedere lo stop dei servizi che seleziona, in ordine inverso alle
dipendenze.

### 3.7 Runtime registry

Metadati persistenti che permettono a invocazioni separate di riconoscere un
processo:

- project e service;
- PID e process group ID;
- process start time o identificatore equivalente;
- comando/config hash;
- working directory;
- timestamp di avvio;
- porta attesa;
- percorsi dei log.

Il PID da solo non è un'identità sufficiente: prima di inviare un segnale `hum`
deve verificare anche start time e metadati disponibili, così da non colpire un
PID riutilizzato.

### 3.8 Process, port e health

Sono segnali distinti:

```text
Process  starting | running | exited | missing | stopping
Port     listening | closed | unknown | occupied-by-other
Health   unchecked | checking | healthy | unhealthy
```

La UI può derivare un'etichetta sintetica, ma non deve perdere i tre valori
sottostanti. Un processo può essere `running`, avere la porta `listening` ed
essere contemporaneamente `unhealthy`.

## 4. Obiettivi

- Avviare un ambiente locale con un solo comando non interattivo.
- Lasciare attivi i servizi dopo l'uscita di CLI e TUI.
- Rendere funzionanti `status`, `stop`, `restart` e `logs` fra invocazioni.
- Selezionare esplicitamente project e template.
- Gestire dipendenze acicliche e readiness configurabile.
- Conservare log persistenti con limiti di spazio.
- Monitorare PID, porte e health con polling leggero.
- Diagnosticare configurazione e runtime stale.
- Consumare risorse trascurabili rispetto ai servizi gestiti.
- Supportare inizialmente macOS e Linux.
- Gestire Compose senza sostituirlo e senza duplicarne il modello.
- Ottenere ambienti da provider dichiarativi senza stampare valori sensibili.
- Conservare ogni conoscenza di prodotto in pack esterni.

## 5. Non obiettivi

La prima release non deve:

- introdurre un daemon centrale `hum` residente;
- riavviare automaticamente servizi dopo un crash;
- sostituire Docker Compose, Kubernetes, launchd o systemd;
- gestire deployment, host remoti o repliche;
- includere una REST API, autenticazione o plugin;
- scaricare ed eseguire configurazioni remote;
- diventare un vault o memorizzare segreti nel proprio schema;
- diventare un package manager o un sistema di scheduling.

I servizi vengono eseguiti in background, ma questa indipendenza non implica
l'esistenza di un processo supervisore `hum`.

## 6. CLI

### 6.1 Grammatica

```text
hum <project> <template> <command> [arguments]
```

Comandi iniziali:

```bash
hum demo all-services start
hum demo all-services stop
hum demo all-services restart
hum demo all-services status
hum demo all-services plan --json
hum demo all-services secrets sync
hum demo all-services logs api --follow
hum demo all-services reset
hum demo all-services doctor
hum demo all-services tui
```

`plan`, `start`, `doctor` e `secrets sync` accettano esclusioni ripetibili:

```bash
hum demo all-services plan --exclude identity
hum demo all-services start --exclude-service mail
```

`--exclude` sottrae i root del template indicato, ma una dipendenza necessaria
può reintrodurli con un warning deterministico. `--exclude-service` è un veto
forte: se un'unità rimasta dipende dal servizio escluso, la selezione fallisce
prima di qualsiasi side effect.

Varianti interattive:

- `hum` apre il selettore di project e template;
- `hum <project>` apre il selettore dei template del progetto;
- `hum <project> <template>` apre la TUI nel contesto selezionato;
- `tui` rende esplicito lo stesso comportamento per script e documentazione.

### 6.2 Semantica

`start`:

1. carica registry e configurazione;
2. risolve template e dipendenze;
3. acquisisce un lock per progetto;
4. riconcilia eventuale stato persistito;
5. avvia i servizi mancanti in process group/sessioni indipendenti;
6. redirige stdin, stdout e stderr prima di restituire;
7. persiste atomicamente i metadati runtime;
8. termina senza lasciare un processo `hum` residente.

`stop` invia `SIGTERM` al process group, attende il timeout configurato e usa
`SIGKILL` solo se necessario. Verifica l'identità del processo prima di inviare
segnali e arresta i servizi in ordine inverso alle dipendenze.

`restart` opera sul processo realmente registrato e non equivale a uno start
cieco da un Manager vuoto.

`status` riconcilia registry e sistema operativo. Non considera il contenuto
del registry una prova sufficiente che il processo sia vivo.

`logs` legge file persistenti e supporta almeno tail e follow.

### 6.3 Exit code

```text
0   operazione completata
1   errore generico
2   configurazione non valida
3   project non trovato
4   template non trovato
5   servizio non trovato
6   avvio fallito o parziale
7   arresto fallito o parziale
8   health/readiness fallita
9   doctor non superato
10  runtime registry incoerente
```

Gli exit code `already-running` e `already-stopped` sono `0` quando il risultato
richiesto è già soddisfatto e l'identità del processo è stata verificata.

## 7. Configurazione

### 7.1 Registry globale

Percorso:

```text
$XDG_CONFIG_HOME/hum/config.yaml
~/.config/hum/config.yaml              # fallback
```

Esempio:

```yaml
version: 1

projects:
  demo:
    config: ~/code/demo/hum.yaml
```

### 7.2 Configurazione progetto

File condiviso `hum.yaml`, con override locale opzionale `hum.local.yaml`. La
v3 aggiunge runtime, provider e task nominati:

```yaml
version: 3
project: demo

runtimes:
  local:
    type: process
  containers:
    type: compose
    project_name: demo-local
    files: [compose.yaml]

environment_providers:
  development:
    type: one-password

repositories:
  applications:
    path: ./demo-applications
  api:
    runtime: local
    path: ./demo-api

services:
  api:
    repository: api
    command: pnpm dev
    port: 3000
    env_file: .env
    healthcheck:
      type: http
      url: http://localhost:3000/health

  database:
    runtime: containers
    target: postgres

  frontend:
    repository: applications
    cwd: apps/procurement-frontend
    command: pnpm dev
    port: 5173
    depends_on:
      - api
      - database

templates:
  all-services:
    services:
      - api
      - frontend
```

Tutti i percorsi relativi sono risolti rispetto al file che li dichiara, non
alla working directory dalla quale viene eseguito `hum`.

Ordine di precedenza:

```text
defaults
  < hum.yaml
  < hum.local.yaml
  < environment
  < CLI arguments
```

Lo schema deve rifiutare campi sconosciuti. `env_file` viene realmente caricato
e i suoi valori hanno precedenza inferiore a `service.env`, ambiente del
processo e override CLI. Gli errori includono file, campo, posizione e hint.

## 8. Stato persistente e locking

Directory predefinita:

```text
$XDG_STATE_HOME/hum/<project>/
~/.local/state/hum/<project>/          # fallback
```

Layout indicativo:

```text
~/.local/state/hum/demo/
├── runtime/
│   ├── api.json
│   └── frontend.json
├── logs/
│   ├── api.stdout.log
│   ├── api.stderr.log
│   └── frontend.stdout.log
└── project.lock
```

Le scritture del registry sono atomiche: file temporaneo nella stessa directory,
flush e rename. Il lock serializza operazioni concorrenti sullo stesso progetto.

### 8.1 Riconciliazione stale

Un'entry è stale quando il processo non esiste o la sua identità non coincide.
`status` la mostra esplicitamente; `doctor` spiega la causa. La pulizia può
essere automatica solo dopo aver verificato che nessun processo corrispondente
possa ricevere segnali per errore.

Un processo trovato sulla porta attesa ma non riconosciuto dal registry è
`occupied-by-other`, non un servizio gestito.

## 9. Gestione dei processi

Ogni servizio usa un process group dedicato. Su macOS e Linux deve essere
indipendente dal terminale e dalla sessione `hum`. stdin è collegato a null;
stdout e stderr sono rediretti su file prima che `start` termini.

Comandi shell nella configurazione sono considerati codice fidato. La
configurazione non viene scaricata automaticamente da sorgenti remote.

`start` è idempotente. Prima di avviare un servizio controlla registry, identità
del PID e porta. Un conflitto non deve produrre un secondo processo.

Il crash di un servizio:

- non avvia un nuovo processo automaticamente;
- resta visibile tramite stato, exit code quando disponibile e log;
- non causa il crash della CLI o TUI;
- può essere recuperato con `restart`.

## 10. Polling e health

Polling indicativo:

- esistenza/identità PID: 500–1000 ms nella TUI;
- porta TCP: 1–2 s o intervallo configurato;
- health HTTP/TCP: intervallo del servizio.

Il controllo PID deve essere mirato. Il controllo porta usa una connessione TCP
breve e non bloccante. I client HTTP vengono riutilizzati.

`lsof` è usato solo per identificare l'occupante di una porta o come fallback
diagnostico. Non deve essere eseguito per ogni servizio a ogni redraw: avviare
processi esterni nel polling ordinario sarebbe più costoso e meno portabile.

I probe sono cancellabili, non si sovrappongono e appartengono a una specifica
generazione del processo. Il risultato di una generazione precedente non può
modificare lo stato dopo un restart.

## 11. Log

I log sono persistenti e separano stdout e stderr. Ogni record visualizzato
include timestamp, servizio, stream e contenuto quando queste informazioni sono
disponibili senza modificare l'output originale su disco.

La retention è limitata per byte e numero di file, non soltanto per righe. Sono
configurabili almeno:

- dimensione massima per file;
- numero di file ruotati;
- limite per singola riga/chunk;
- pattern da mascherare nella visualizzazione.

La TUI legge incrementi dai file e non conserva l'intera cronologia in RAM.
`logs --follow` continua a funzionare anche se la TUI non è aperta.

## 12. TUI

La TUI mostra il project e template selezionati e, per ogni servizio:

- process state e PID/PGID;
- uptime;
- porta e port state;
- health state e ultimo risultato;
- exit code o errore;
- accesso ai log persistenti.

Shortcut iniziali:

```text
↑/k, ↓/j   navigazione
space      start/stop esplicito
r          restart
enter      dettagli
l          log
d          doctor
o          apri URL
?          help
q          chiudi la TUI senza fermare i servizi
```

Le operazioni lente e `doctor` non bloccano l'event loop. Gli errori delle
azioni vengono mostrati, non ignorati. Uno stop globale richiede una scelta
esplicita; la semplice chiusura della TUI non è uno stop.

## 13. Doctor

`doctor` controlla:

- registry globale e configurazione progetto;
- repository, working directory e comandi richiesti;
- file ed env file richiesti;
- variabili d'ambiente senza mostrarne il valore;
- dipendenze e cicli;
- runtime registry stale;
- identità dei processi registrati;
- porte chiuse, gestite o occupate da processi estranei;
- directory di stato/log e permessi.

Una porta occupata dal servizio `hum` atteso non è un errore. Una porta occupata
da un processo non riconosciuto è una diagnosi distinta.

## 14. Requisiti non funzionali

- Distribuzione come singolo binario.
- TUI visibile entro circa 200 ms con configurazione locale valida.
- Con dieci servizi, overhead CPU e RSS documentato e non significativo.
- Nessuna crescita RAM proporzionale alla cronologia dei log.
- Nessuna scansione completa dei processi nel polling ordinario.
- Crash e output elevato di un servizio non devono compromettere `hum`.
- Supporto iniziale macOS e Linux.
- Errori con contesto e azione suggerita.
- Configurazioni semplici, leggibili e versionabili.

## 15. Sicurezza

- Verificare identità e start time prima di segnalare un PID registrato.
- Usare permessi utente per stato e log.
- Non stampare valori di variabili sensibili.
- Non scrivere valori sensibili nei file Compose generati.
- Scrivere cache provider solo se configurate, atomicamente e con modo `0600`.
- `doctor` verifica la disponibilità del provider senza leggere item.
- Mascherare pattern configurati nella visualizzazione dei log.
- Considerare i comandi locali configurati come codice fidato.
- Non eseguire configurazioni remote automaticamente.
- Non modificare file `.env` posseduti dal progetto; le cache provider hanno
  percorsi distinti, espliciti e gitignored.

## 16. Migrazione delle configurazioni

La v1 usava `profiles` e una CLI implicita basata sulla discovery del file. La
v2 richiede:

1. registrare il project nel registry globale;
2. aggiungere `project` al file di progetto;
3. cambiare `version: 1` in `version: 2`;
4. rinominare `profiles` in `templates`;
5. sostituire `hum up <profile>` con
   `hum <project> <template> start`.

Una configurazione v1 deve produrre un errore di migrazione leggibile. Non viene
avviata implicitamente in modalità legacy, perché ciò reintrodurrebbe ownership
e semantiche incompatibili.

La v3 mantiene v1/v2 process-only e richiede che ogni servizio v3 scelga un
runtime nominato. La migrazione da un CLI di prodotto sposta Compose, immagini,
vault reference e bootstrap in un repository/pack del prodotto. Il core `hum`
non importa moduli né manifest specifici di quel pack.

Un runtime Compose può elencare layer `generated_files` prodotti da task del
pack. I file assenti non invalidano `doctor`; quando esistono vengono aggiunti
alla CLI Compose senza interpretarne il contenuto nel core.

## 17. Criteri di accettazione

### CLI e configurazione

- `hum demo all-services start` funziona da qualunque directory.
- Project e template sconosciuti hanno errori distinti.
- Due template sovrapposti non duplicano un servizio.
- Campi sconosciuti e path errati sono diagnosticati.
- Lo stesso grafo ordina processi, servizi Compose e task senza cicli.
- `plan` non contatta Docker o provider e non mostra valori d'ambiente.
- Le esclusioni di template segnalano le dipendenze reintrodotte; le esclusioni
  di servizio bloccano dipendenze incompatibili prima di ogni side effect.

### Lifecycle

- Dopo `start` non rimane un processo `hum` residente.
- Una nuova invocazione vede, riavvia e ferma i servizi avviati prima.
- Stop termina l'intero process group senza rischiare PID riutilizzati.
- Entry stale e fallimenti parziali sono visibili e recuperabili.

### Osservabilità

- Process, port e health restano distinti.
- La TUI osserva processi già attivi e non ne determina il lifetime.
- Log e stato restano disponibili dopo l'uscita della TUI.
- Il polling ordinario non esegue `lsof` o scansioni complete del sistema.

### Qualità

- Test di integrazione coprono invocazioni separate e processi reali fixture.
- `cargo fmt --check`, Clippy, test e build release sono gate CI.
- Test e benchmark non lasciano processi o stato residuo.
- Un test con Docker fake verifica project isolation, start/stop/status/logs,
  reset, task idempotenti e assenza di segreti nell'override generato.

## 18. Release iniziale proposta

La release è pronta quando uno sviluppatore può configurare Demo ed eseguire:

```bash
hum demo all-services doctor
hum demo all-services start
hum demo all-services status
hum demo all-services tui
```

Chiudere la TUI lascia i servizi attivi. In seguito:

```bash
hum demo all-services logs api --follow
hum demo all-services stop
```

Lo stesso modello deve essere applicabile a un secondo progetto aggiungendo una
sola entry al registry globale e un file `hum.yaml` versionabile.
