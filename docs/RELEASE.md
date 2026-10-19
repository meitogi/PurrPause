# Publier une release PurrPause

Checklist pour sortir une version `X.Y.Z` sur GitHub, des sources jusqu'aux
deux zips téléchargeables. Remplace `X.Y.Z` par la version partout (liens
compris) ; `P.Q.R` est la release précédente.

Dépôt : <https://github.com/meitogi/PurrPause>

| Étape | Qui | Où |
| --- | --- | --- |
| 1. Branche fusionnée dans `main` | toi | GitHub (PR) |
| 2. Bump de version | Claude | devcontainer |
| 3. Tests + checks | Claude | devcontainer |
| 4. Build des zips | Claude | devcontainer |
| 5. Tag annoté | Claude | devcontainer |
| 6. Push `main` + tag | toi | Fork |
| 7. Formulaire de release + zips | toi | GitHub (web) |
| 8. Vérification | toi | GitHub (web) |

## 1. Fusionner la branche dans `main`

Une release part toujours de `main`. Ouvre la PR de ta branche de travail :

- Nouvelle PR : <https://github.com/meitogi/PurrPause/compare/main...NOM-DE-BRANCHE>
- PRs ouvertes : <https://github.com/meitogi/PurrPause/pulls>

Après le merge, mets `main` à jour en local (Fork → Pull sur `main`).

## 2. Bump de version

La version vit dans `Cargo.toml` (`version = "X.Y.Z"`). Le `build.rs` la lit
pour le VERSIONINFO de l'exe, et `pack` pour le nom des zips : rien d'autre à
toucher côté code.

À aligner à la main dans la doc :

- [README.md](../README.md) — ligne `**Status** : vX.Y.Z`.
- [SECURITY-NOTES.md](SECURITY-NOTES.md) — titre `(PurrPause vX.Y.Z)`.

Format : `X.Y.Z` pour une stable, `X.Y.Z-b.N` pour une bêta.

```bash
cargo check
grep -rn 'version = ' Cargo.toml
```

`cargo check` met à jour `Cargo.lock` avec la nouvelle version ; committe
les deux fichiers ensemble (`chore: bump to X.Y.Z`).

## 3. Tests et checks

Les deux cibles doivent compiler sans warning :

```bash
cargo test
cargo xwin check --release --target x86_64-pc-windows-msvc
cargo xwin check --release --target aarch64-pc-windows-msvc
```

Un test rouge ou un warning = pas de release.

## 4. Build des zips

```bash
wtf c
```

(équivaut à `cargo run --bin pack --release`.) Résultat dans `target/dist/` :

- `SystemHealthAgent-X.Y.Z-x64.zip`
- `SystemHealthAgent-X.Y.Z-aarch64.zip`

Contrôle du contenu — chaque zip doit lister l'exe, son `.manifest`,
`LICENSE`, `NOTICE.md` et `scripts/*.bat` :

```bash
unzip -l target/dist/SystemHealthAgent-X.Y.Z-x64.zip
unzip -l target/dist/SystemHealthAgent-X.Y.Z-aarch64.zip
```

`LICENSE` et `NOTICE.md` sont obligatoires (licences MIT et Lottie Simple
License) : `pack` échoue s'ils manquent.

Idéalement, un smoke rapide sur une machine Windows avant de publier :
extraire, lancer, vérifier que le popup et la Config UI s'ouvrent.

## 5. Tag annoté

Sur le commit de bump, dans ce dépôt :

```bash
git tag -a vX.Y.Z -m "PurrPause vX.Y.Z"
git show vX.Y.Z --stat
```

Préfixe `v` obligatoire, comme `v1.0.0-b.1`.

## 6. Push (Fork)

Dans Fork, sur ce dépôt :

1. Push de `main`.
2. Push du tag : coche l'option de push des tags dans la fenêtre de push,
   ou clic droit sur le tag `vX.Y.Z` dans la barre latérale → push vers
   `origin`.

Contrôle : le tag apparaît sur <https://github.com/meitogi/PurrPause/tags>.

## 7. Créer la release sur GitHub

Formulaire pré-rempli (tag + titre) :

- Stable :
  <https://github.com/meitogi/PurrPause/releases/new?tag=vX.Y.Z&title=PurrPause%20vX.Y.Z>
- Bêta (case *pre-release* déjà cochée) :
  <https://github.com/meitogi/PurrPause/releases/new?tag=vX.Y.Z-b.N&title=PurrPause%20vX.Y.Z-b.N&prerelease=1>

Dans le formulaire :

1. Vérifie que le tag affiché est bien le tag existant (pas « Create new
   tag on publish ») — sinon le push de l'étape 6 n'est pas passé.
2. **Generate release notes** pour une base de notes depuis la release
   précédente, puis retouche : ce qui change pour l'utilisateur, en
   premier. Le diff complet sert de référence :
   <https://github.com/meitogi/PurrPause/compare/vP.Q.R...vX.Y.Z>
3. Rappelle dans les notes le choix du zip : `x64` pour un PC classique,
   `aarch64` pour Snapdragon / ARM64, et l'extraction dans un dossier
   utilisateur (voir [README.md](../README.md) § Install).
4. Glisse les deux zips de `target/dist/` dans la zone **Attach binaries**.
5. Coche **Set as the latest release** pour une stable, **Set as a
   pre-release** pour une bêta.
6. **Publish release**.

## 8. Vérifier

- Page de la release : <https://github.com/meitogi/PurrPause/releases/tag/vX.Y.Z>
- Dernière release : <https://github.com/meitogi/PurrPause/releases/latest>
- Liens de téléchargement directs (à tester une fois) :
  - <https://github.com/meitogi/PurrPause/releases/download/vX.Y.Z/SystemHealthAgent-X.Y.Z-x64.zip>
  - <https://github.com/meitogi/PurrPause/releases/download/vX.Y.Z/SystemHealthAgent-X.Y.Z-aarch64.zip>

## Corriger ou annuler

- **Notes ou zips à changer** : <https://github.com/meitogi/PurrPause/releases/edit/vX.Y.Z>
  — on peut supprimer un asset et en reposer un autre sans toucher au tag.
- **Release ratée** : sur la page d'édition, **Delete** supprime la release
  mais laisse le tag. Supprime ensuite le tag distant dans Fork (clic droit
  sur le tag → suppression, en incluant `origin`), puis le tag local s'il
  reste :

  ```bash
  git tag -d vX.Y.Z
  ```

  Corrige, puis reprends à l'étape 3. Si la version a déjà été téléchargée,
  préfère sortir la version suivante (patch + 1) plutôt que de réutiliser le même numéro.
